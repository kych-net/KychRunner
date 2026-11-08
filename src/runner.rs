//! 单个 job 的状态机：claim → 工作树 → start → act → 回写 → complete。
//!
//! protocol v2 直接读写 `/git/<项目>/`：代码用 `archive?ref=commit_oid`，
//! 定义用 `tree` + `file` 且恒取 `def_commit_oid`；产物按清单显式回写。
//! claim 未下发 `git_base` 时回退到 v1 的打包快照端点。

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::act::{self, ActContext, OutputLine};
use crate::client::Client;
use crate::config::Config;
use crate::job::{Claim, CompleteReport, LogChunk, StartReport, StepReport};
use crate::log::{LogBatcher, Redactor};
use crate::push;
use crate::sandbox::{self, Layout};
use crate::snapshot::{self, UnpackLimits};

pub async fn execute(cfg: Arc<Config>, client: Arc<Client>, claim: Claim) -> Result<()> {
    let attempt = claim.attempt;
    let run_id = claim.run_id.clone();
    let layout = sandbox::create_layout(&cfg.work_root, &run_id)?;

    let result = run_inner(cfg.clone(), client.clone(), &claim, &layout).await;

    if let Err(ref e) = result {
        // 尽早回报失败，让主服务直接写 failure，而不是等租约超时把 job 打回队列。
        let report = CompleteReport {
            attempt,
            conclusion: "failure".to_string(),
            exit_code: None,
            error: Some(format!("{e:#}").chars().take(500).collect()),
        };
        if let Err(e2) = client.complete(&run_id, &report).await {
            warn!("回报失败状态出错：{e2:#}");
        }
    }

    if let Err(e) = sandbox::cleanup(&layout.run_dir) {
        warn!("清理 {} 失败：{e:#}", layout.run_dir.display());
    }
    result
}

async fn run_inner(
    cfg: Arc<Config>,
    client: Arc<Client>,
    claim: &Claim,
    layout: &Layout,
) -> Result<()> {
    let run_id = &claim.run_id;
    let attempt = claim.attempt;
    info!(
        "run {run_id} 租约至 {:?}，容器网络={}（执行器强制 none）",
        claim.lease_until, claim.limits.network
    );
    if claim.limits.network != "none" {
        warn!(
            "主服务下发 network={}，执行器仍按 none 断网执行",
            claim.limits.network
        );
    }

    let max_snapshot = claim.limits.max_snapshot_bytes.min(cfg.max_snapshot_bytes);
    let limits = UnpackLimits {
        max_bytes: max_snapshot,
        max_entries: 50_000,
        max_depth: 32,
    };

    let stats = if let Some(git_base) = claim.git_base.as_deref().filter(|b| !b.is_empty()) {
        // v2：代码归档按 commit_oid 精确取，避免排队期间分支被新 push 顶掉。
        let meta = client
            .download_archive(
                git_base,
                &claim.commit_oid,
                &layout.snapshot_path,
                max_snapshot,
            )
            .await
            .context("下载 /git/ 工作树归档失败")?;
        info!(
            "工作树归档：files={:?} bytes={:?} commit={:?}",
            meta.files, meta.bytes, meta.commit_oid
        );
        fetch_definitions(
            client.as_ref(),
            git_base,
            &claim.def_commit_oid,
            &claim.project.workflow_dir,
            &layout.defs_dir,
        )
        .await
        .context("读取默认分支工作流定义失败")?;

        let gz = layout.snapshot_path.clone();
        let ws = layout.workspace.clone();
        let lim = limits.clone();
        tokio::task::spawn_blocking(move || snapshot::unpack_plain(&gz, &ws, &lim))
            .await
            .context("解包工作树归档任务崩溃")??
    } else {
        // v1 兼容回退：主服务没下发 git_base 时仍走打包快照端点。
        warn!("claim 未提供 git_base，回退到 v1 /api/runner/jobs/{{id}}/snapshot");
        let meta = client
            .download_snapshot(run_id, &layout.snapshot_path, max_snapshot)
            .await
            .context("下载兼容快照失败")?;
        info!(
            "兼容快照：files={:?} bytes={:?} commit={:?} def_commit={:?}",
            meta.files, meta.bytes, meta.commit_oid, meta.def_commit_oid
        );
        let gz = layout.snapshot_path.clone();
        let ws = layout.workspace.clone();
        let defs = layout.defs_dir.clone();
        let lim = limits.clone();
        tokio::task::spawn_blocking(move || snapshot::unpack(&gz, &ws, &defs, &lim))
            .await
            .context("解包兼容快照任务崩溃")??
    };
    info!(
        "工作区就绪：code_files={} definitions={}",
        stats.code_files,
        stats.definition_files.len()
    );

    let workflow_path = match claim.workflow_path.as_deref() {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => {
            let rel = snapshot::pick_definition(&layout.defs_dir)?;
            format!(
                "{}/{}",
                claim.project.workflow_dir.trim_end_matches('/'),
                rel
            )
        }
    };

    sandbox::install_definitions(
        &layout.defs_dir,
        &layout.workspace,
        &claim.project.workflow_dir,
    )?;
    if !layout.workspace.join(&workflow_path).is_file() {
        bail!("默认分支定义里没有工作流文件 {}", workflow_path);
    }

    client
        .start(
            run_id,
            &StartReport {
                attempt,
                workflow_path: workflow_path.clone(),
                jobs: Vec::new(),
            },
        )
        .await
        .context("回报 start 失败")?;

    sandbox::write_event_json(&layout.event_json, claim)?;
    sandbox::write_env_allowlist(&layout.env_file, claim)?;
    let secrets = sandbox::write_secrets(&layout.secret_file, claim)?;

    let act_ctx = ActContext {
        act_bin: cfg.act_bin.clone(),
        workspace: layout.workspace.clone(),
        workflow_path: workflow_path.clone(),
        event_name: claim.event.name.clone(),
        event_json: layout.event_json.clone(),
        env_file: layout.env_file.clone(),
        secret_file: layout.secret_file.clone(),
        actions_dir: cfg.actions_dir.clone(),
        act_cache: cfg.act_cache.clone(),
        runner_image: cfg.runner_image.clone(),
        default_branch: claim.project.default_branch.clone(),
        actor: claim.actor.clone().unwrap_or_else(|| "kychnet".to_string()),
        timeout_secs: claim.limits.timeout_secs.min(cfg.timeout_secs),
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<OutputLine>();
    let handle = act::spawn(act_ctx.clone(), tx);

    let redactor = Redactor::new(&secrets);
    let max_log = claim.limits.max_log_bytes.min(cfg.max_log_bytes) as usize;
    let mut batcher = LogBatcher::new(32 * 1024, 20, max_log);
    let mut seq: u64 = 0;
    let mut logged: usize = 0;
    let mut step_buf: Vec<StepReport> = Vec::new();

    while let Some(line) = rx.recv().await {
        match line {
            OutputLine::Log(raw) => {
                let red = redactor.redact(&raw);
                if logged.saturating_add(red.len() + 1) > max_log {
                    continue; // 截断，不无限回报日志
                }
                logged += red.len() + 1;
                if let Some(lines) = batcher.push(red) {
                    seq += 1;
                    if let Err(e) = client
                        .logs(
                            run_id,
                            &LogChunk {
                                attempt,
                                seq,
                                lines,
                            },
                        )
                        .await
                    {
                        warn!("上报日志分片失败：{e:#}");
                    }
                }
            }
            OutputLine::Step(s) => {
                step_buf.push(StepReport {
                    job_id: s.job_id,
                    step_index: s.step_index,
                    name: s.name,
                    status: s.status,
                    exit_code: s.exit_code,
                });
                if step_buf.len() >= 16 {
                    flush_steps(client.as_ref(), run_id, attempt, &mut step_buf).await;
                }
            }
        }
    }

    let lines = batcher.take();
    if !lines.is_empty() {
        seq += 1;
        if let Err(e) = client
            .logs(
                run_id,
                &LogChunk {
                    attempt,
                    seq,
                    lines,
                },
            )
            .await
        {
            warn!("上报末尾日志失败：{e:#}");
        }
    }
    flush_steps(client.as_ref(), run_id, attempt, &mut step_buf).await;

    let outcome = handle.await.context("act 任务崩溃")??;

    // 显式回写：只有 .kychnet/push.json 里列出的路径才提交，且只在 act
    // 成功时执行。容器 --network=none，回写由 runner 宿主进程代做。
    let mut push_error: Option<String> = None;
    if outcome.exit_code == 0 && !outcome.timed_out {
        match push::apply(client.as_ref(), claim, &layout.workspace).await {
            Ok(result) if result.written + result.deleted > 0 => info!(
                "产物回写完成：written={} deleted={} bytes={}",
                result.written, result.deleted, result.bytes
            ),
            Ok(_) => {}
            Err(error) => {
                warn!("产物回写失败：{error:#}");
                push_error = Some(format!("{error:#}").chars().take(500).collect());
            }
        }
    }

    let (conclusion, error) = if outcome.timed_out {
        (
            "failure".to_string(),
            Some(format!("act 执行超过 {}s，已终止", cfg.timeout_secs)),
        )
    } else if let Some(detail) = push_error {
        (
            "failure".to_string(),
            Some(format!("产物回写失败：{detail}")),
        )
    } else if outcome.exit_code == 0 {
        ("success".to_string(), None)
    } else {
        (
            "failure".to_string(),
            Some(format!("act 退出码 {}", outcome.exit_code)),
        )
    };

    info!(
        "run {run_id} 结束：{conclusion} (exit={})",
        outcome.exit_code
    );
    client
        .complete(
            run_id,
            &CompleteReport {
                attempt,
                conclusion,
                exit_code: Some(outcome.exit_code),
                error,
            },
        )
        .await
        .context("回报 complete 失败")?;
    Ok(())
}

/// 从 `/git/<项目>/tree` + `/file` 递归拉取默认分支 head 的 workflow_dir 下
/// 所有 *.yml / *.yaml。恒用 `def_commit_oid`，保证 PR 分支里被篡改的工作流
/// 定义不会被执行（与 v1 快照的 definitions/ 语义一致）。
async fn fetch_definitions(
    client: &Client,
    git_base: &str,
    def_commit: &str,
    workflow_dir: &str,
    defs_dir: &Path,
) -> Result<()> {
    tokio::fs::create_dir_all(defs_dir)
        .await
        .context("创建 definitions 目录失败")?;
    let root = workflow_dir.trim_matches('/');
    let prefix = format!("{root}/");
    let mut stack = vec![root.to_string()];
    let mut found = 0usize;

    while let Some(dir) = stack.pop() {
        let items = client
            .git_tree(git_base, def_commit, &dir)
            .await
            .with_context(|| format!("列目录 {dir} 失败"))?;
        for item in items {
            if item.is_dir {
                stack.push(item.path);
                continue;
            }
            if !(item.path.ends_with(".yml") || item.path.ends_with(".yaml")) {
                continue;
            }
            let rel = item
                .path
                .strip_prefix(prefix.as_str())
                .unwrap_or(&item.name);
            if !valid_rel(rel) {
                bail!("工作流定义路径非法：{}", item.path);
            }
            let data = client
                .git_file(git_base, def_commit, &item.path)
                .await
                .with_context(|| format!("读取 {} 失败", item.path))?;
            let dest = defs_dir.join(rel);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("创建 {} 失败", parent.display()))?;
            }
            tokio::fs::write(&dest, &data)
                .await
                .with_context(|| format!("写入 {} 失败", dest.display()))?;
            found += 1;
        }
    }
    if found == 0 {
        bail!("默认分支 {workflow_dir} 下没有工作流定义");
    }
    Ok(())
}

fn valid_rel(value: &str) -> bool {
    if value.is_empty() || value.contains('\\') {
        return false;
    }
    let parts: Vec<&str> = value.split('/').collect();
    parts.len() <= 32 && parts.iter().all(|p| snapshot::valid_name(p))
}

async fn flush_steps(client: &Client, run_id: &str, attempt: i32, buf: &mut Vec<StepReport>) {
    if buf.is_empty() {
        return;
    }
    let steps = std::mem::take(buf);
    if let Err(e) = client.steps(run_id, steps, attempt).await {
        warn!("上报 steps 失败：{e:#}");
    }
}
