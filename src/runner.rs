//! 单个 job 的状态机：claim → 快照 → start → act → logs/steps → complete。

use anyhow::{bail, Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::act::{self, ActContext, OutputLine};
use crate::client::Client;
use crate::config::Config;
use crate::job::{Claim, CompleteReport, LogChunk, StartReport, StepReport};
use crate::log::{LogBatcher, Redactor};
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
    let meta = client
        .download_snapshot(run_id, &layout.snapshot_path, max_snapshot)
        .await
        .context("下载模式 B 快照失败")?;
    info!(
        "快照已下载：files={:?} bytes={:?} commit={:?} def_commit={:?}",
        meta.files, meta.bytes, meta.commit_oid, meta.def_commit_oid
    );

    let gz = layout.snapshot_path.clone();
    let ws = layout.workspace.clone();
    let defs = layout.defs_dir.clone();
    let limits = UnpackLimits {
        max_bytes: max_snapshot,
        max_entries: 50_000,
        max_depth: 32,
    };
    let stats = tokio::task::spawn_blocking(move || snapshot::unpack(&gz, &ws, &defs, &limits))
        .await
        .context("解包任务崩溃")??;
    info!(
        "快照解包完成：code_files={} definitions={}",
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

    let (conclusion, error) = if outcome.timed_out {
        (
            "failure".to_string(),
            Some(format!("act 执行超过 {}s，已终止", cfg.timeout_secs)),
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

async fn flush_steps(client: &Client, run_id: &str, attempt: i32, buf: &mut Vec<StepReport>) {
    if buf.is_empty() {
        return;
    }
    let steps = std::mem::take(buf);
    if let Err(e) = client.steps(run_id, steps, attempt).await {
        warn!("上报 steps 失败：{e:#}");
    }
}
