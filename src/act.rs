//! 调用 nektos/act 执行 GitHub Actions 语法工作流。
//! 主服务不装 Docker、不解析 YAML；这里也只把 act 当外部 CLI 驱动。

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub enum OutputLine {
    Log(String),
    Step(StepEvent),
}

#[derive(Debug, Clone)]
pub struct StepEvent {
    pub job_id: String,
    pub step_index: i32,
    pub name: String,
    pub status: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct ActOutcome {
    pub exit_code: i32,
    pub timed_out: bool,
}

#[derive(Debug, Clone)]
pub struct ActContext {
    pub act_bin: PathBuf,
    pub workspace: PathBuf,
    /// 相对 workspace 的单个工作流文件，例如 .github/workflows/site.yml
    pub workflow_path: String,
    pub event_name: String,
    pub event_json: PathBuf,
    pub env_file: PathBuf,
    pub secret_file: PathBuf,
    pub actions_dir: PathBuf,
    pub act_cache: PathBuf,
    pub runner_image: String,
    pub default_branch: String,
    pub actor: String,
    pub timeout_secs: u64,
}

pub fn spawn(
    ctx: ActContext,
    tx: mpsc::UnboundedSender<OutputLine>,
) -> tokio::task::JoinHandle<Result<ActOutcome>> {
    tokio::spawn(async move { run(ctx, tx).await })
}

async fn run(ctx: ActContext, tx: mpsc::UnboundedSender<OutputLine>) -> Result<ActOutcome> {
    let container_options = "--network=none --memory=2g --cpus=2 --pids-limit=256 \
                             --cap-drop=ALL --security-opt=no-new-privileges";

    let mut cmd = Command::new(&ctx.act_bin);
    cmd.current_dir(&ctx.workspace)
        .arg(&ctx.event_name)
        // 一次 run 只跑 claim 指定的那一个工作流，避免默认分支里多个 yml 全被触发。
        .arg("-W")
        .arg(&ctx.workflow_path)
        .arg("-e")
        .arg(&ctx.event_json)
        .arg("-a")
        .arg(&ctx.actor)
        .arg("--defaultbranch")
        .arg(&ctx.default_branch)
        .arg("-P")
        .arg(format!("ubuntu-latest={}", ctx.runner_image))
        // 断网：uses: 只允许预置的离线缓存，缺失直接失败而不是联网拉取。
        .arg("--action-offline-mode")
        .arg("--action-cache-path")
        .arg(&ctx.act_cache)
        // 代码已由快照铺好，checkout 映射为 no-op；工作区本来就没有 .git。
        .arg("--local-repository")
        .arg(format!(
            "actions/checkout@v4={}",
            ctx.actions_dir.join("noop-checkout").display()
        ))
        // 必须显式覆盖：act 默认会读工作目录下的 .env 注入容器。
        .arg("--env-file")
        .arg(&ctx.env_file)
        .arg("--secret-file")
        .arg(&ctx.secret_file)
        // 禁止把宿主 Docker socket 挂进 job 容器。
        // 若所用 act 版本不接受 `-` 占位，可移除该参数，由 systemd 的
        // NoNewPrivileges / ProtectSystem 与「不挂载 socket」共同兜底。
        .arg("--container-daemon-socket")
        .arg("-")
        .arg("--container-options")
        .arg(container_options)
        .arg("--network")
        .arg("none")
        .arg("--no-cache-server")
        .arg("--concurrent-jobs")
        .arg("1")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    debug!("act 命令：{:?}", cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("启动 act 失败：{}", ctx.act_bin.display()))?;
    let stdout = child.stdout.take().context("act stdout 未捕获")?;
    let stderr = child.stderr.take().context("act stderr 未捕获")?;

    let tx_out = tx.clone();
    let tx_err = tx.clone();
    let p1 = tokio::spawn(async move { pump(stdout, tx_out).await });
    let p2 = tokio::spawn(async move { pump(stderr, tx_err).await });

    let body = async move {
        let status = child.wait().await.context("等待 act 退出失败")?;
        let _ = p1.await;
        let _ = p2.await;
        Ok::<_, anyhow::Error>(ActOutcome {
            exit_code: status.code().unwrap_or(-1),
            timed_out: false,
        })
    };

    // 超时后 body 被 drop，kill_on_drop(true) 会杀掉 act 进程；
    // pump 任务随管道 EOF 自行结束，之后显式中止以免泄漏。
    match timeout(Duration::from_secs(ctx.timeout_secs.max(1)), body).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            warn!("act 执行超过 {}s，已终止", ctx.timeout_secs);
            Ok(ActOutcome {
                exit_code: -1,
                timed_out: true,
            })
        }
    }
}

async fn pump<R>(reader: R, tx: mpsc::UnboundedSender<OutputLine>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if tx.send(parse_line(&line)).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// act --json 输出按行解析；任何无法识别的行都当普通日志，绝不因格式差异丢日志。
fn parse_line(line: &str) -> OutputLine {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(step) = parse_step_event(&v) {
                return OutputLine::Step(step);
            }
            let msg = v
                .get("msg")
                .or_else(|| v.get("message"))
                .and_then(|x| x.as_str())
                .unwrap_or(trimmed);
            return OutputLine::Log(msg.to_string());
        }
    }
    OutputLine::Log(line.to_string())
}

fn parse_step_event(v: &serde_json::Value) -> Option<StepEvent> {
    let job_id = v.get("job").and_then(|x| x.as_str())?.to_string();
    let step_index = v
        .get("step_index")
        .or_else(|| v.get("stepIndex"))
        .and_then(|x| x.as_i64())
        .or_else(|| v.get("step").and_then(|x| x.as_i64()))
        .unwrap_or(0) as i32;
    let name = v
        .get("step_name")
        .or_else(|| v.get("name"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let status = v
        .get("status")
        .and_then(|x| x.as_str())
        .unwrap_or("in_progress")
        .to_string();
    let exit_code = v
        .get("exit_code")
        .or_else(|| v.get("exitCode"))
        .and_then(|x| x.as_i64())
        .map(|x| x as i32);
    Some(StepEvent {
        job_id,
        step_index,
        name,
        status,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_stay_logs() {
        match parse_line("[build] cargo test --locked") {
            OutputLine::Log(s) => assert!(s.contains("cargo test")),
            _ => panic!("普通行不应被解析成 step"),
        }
    }

    #[test]
    fn json_msg_becomes_log() {
        match parse_line(r#"{"level":"info","msg":"hello"}"#) {
            OutputLine::Log(s) => assert_eq!(s, "hello"),
            _ => panic!("无 job 字段的 JSON 应视为日志"),
        }
    }

    #[test]
    fn json_with_job_becomes_step() {
        match parse_line(
            r#"{"job":"build","step_index":2,"name":"Run tests","status":"success","exit_code":0}"#,
        ) {
            OutputLine::Step(s) => {
                assert_eq!(s.job_id, "build");
                assert_eq!(s.step_index, 2);
                assert_eq!(s.status, "success");
                assert_eq!(s.exit_code, Some(0));
            }
            _ => panic!("含 job 的 JSON 应解析为 step"),
        }
    }
}
