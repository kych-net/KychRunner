mod act;
mod client;
mod config;
mod gitcmd;
mod job;
mod log;
mod push;
mod runner;
mod sandbox;
mod snapshot;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::{Cli, Command, Config, DATA_CHANNEL, PROTOCOL_VERSION};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("kychrunner: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kychrunner=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!(
                "kychrunner {} (protocol {}, data channel {}, snapshot source {})",
                env!("CARGO_PKG_VERSION"),
                PROTOCOL_VERSION,
                DATA_CHANNEL,
                config::SNAPSHOT_SOURCE
            );
        }
        Command::Register {
            url,
            code,
            name,
            labels,
        } => {
            let resp = client::Client::register(&url, &name, &code, &config::parse_labels(&labels))
                .await
                .context("注册失败")?;
            println!("runner_id : {}", resp.runner_id);
            println!("runner_token: {}", resp.token);
            println!();
            println!("token 仅回显这一次。请立刻写入 /etc/kychrunner/env 的 RUNNER_TOKEN=…");
            println!("然后启动：systemctl enable --now kychrunner");
        }
        Command::Run { cfg } => {
            let cfg = Arc::new(cfg);
            doctor(cfg.as_ref(), true).await?;
            run_loop(cfg).await?;
        }
        Command::Git { action, cfg } => {
            gitcmd::run(&cfg, action)
                .await
                .context("Git 工作树操作失败")?;
        }
        Command::Doctor { cfg } => {
            doctor(&cfg, false).await?;
            println!("环境自检完成（警告项见上方日志）");
        }
    }
    Ok(())
}

async fn doctor(cfg: &Config, strict: bool) -> Result<()> {
    if strict {
        cfg.token()?;
    }

    let act_ok = match tokio::process::Command::new(&cfg.act_bin)
        .arg("--version")
        .output()
        .await
    {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            info!("act: {}", v.lines().next().unwrap_or("unknown").trim());
            true
        }
        Ok(o) => {
            warn!("act --version 退出码 {:?}", o.status.code());
            false
        }
        Err(e) => {
            warn!("act 不可用（{}）：{e}", cfg.act_bin.display());
            false
        }
    };
    if strict && !act_ok {
        bail!("act 不可用，无法执行工作流：{}", cfg.act_bin.display());
    }

    tokio::fs::create_dir_all(&cfg.work_root)
        .await
        .with_context(|| format!("创建 {} 失败", cfg.work_root.display()))?;
    let probe = cfg.work_root.join(".write-probe");
    tokio::fs::write(&probe, b"ok")
        .await
        .context("WORK_ROOT 不可写")?;
    tokio::fs::remove_file(&probe).await.ok();

    if !cfg.actions_dir.join("noop-checkout/action.yml").is_file() {
        warn!(
            "缺少 {}；actions/checkout@v4 的 --local-repository 映射会失败",
            cfg.actions_dir.join("noop-checkout/action.yml").display()
        );
    }
    if !cfg.act_cache.is_dir() {
        warn!("act 离线缓存目录不存在：{}", cfg.act_cache.display());
    }
    match tokio::process::Command::new("docker")
        .arg("info")
        .output()
        .await
    {
        Ok(o) if o.status.success() => info!("Docker 可用"),
        _ => warn!("Docker 不可用，act 将无法启动 job 容器"),
    }
    Ok(())
}

pub async fn act_version(cfg: &Config) -> String {
    match tokio::process::Command::new(&cfg.act_bin)
        .arg("--version")
        .output()
        .await
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or("unknown")
            .trim()
            .to_string(),
        Err(_) => "unavailable".to_string(),
    }
}

async fn run_loop(cfg: Arc<Config>) -> Result<()> {
    let token = cfg.token()?.to_string();
    let client = Arc::new(client::Client::new(&cfg.base_url(), &token)?);
    let labels = cfg.labels();
    let version = act_version(cfg.as_ref()).await;
    info!(
        "KychRunner 启动：url={} protocol={} data_channel={} labels={:?}",
        cfg.base_url(),
        PROTOCOL_VERSION,
        DATA_CHANNEL,
        labels
    );

    let stop = Arc::new(AtomicBool::new(false));
    let heartbeat = {
        let client = client.clone();
        let cfg = cfg.clone();
        let labels = labels.clone();
        let version = version.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            // 立刻发一次，让主服务马上知道执行器上线。
            if let Err(e) = client.heartbeat(&version, &labels).await {
                warn!("首次心跳失败：{e:#}");
            }
            loop {
                tokio::time::sleep(Duration::from_secs(cfg.heartbeat_secs.max(5))).await;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = client.heartbeat(&version, &labels).await {
                    warn!("心跳失败：{e:#}");
                }
            }
        })
    };

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let term_fut = sigterm.recv();
    #[cfg(not(unix))]
    let term_fut = std::future::pending::<()>();
    tokio::pin!(term_fut);

    // claim 连续失败退避：失败后先等 1s，之后翻倍封顶 60s；成功即复位。
    // 持续失败（如主站升级后协议不匹配返回 400）不会无限刷日志。
    let mut claim_backoff = Duration::ZERO;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { info!("收到 SIGINT，退出"); break; }
            _ = &mut term_fut => { info!("收到 SIGTERM，退出"); break; }
            _ = tokio::time::sleep(claim_backoff), if claim_backoff > Duration::ZERO => {
                // 退避窗口也放在 select 里，保证信号即时退出。
                claim_backoff = claim_backoff.saturating_mul(2).min(Duration::from_secs(60));
                continue;
            }
            res = client.claim(&labels, cfg.claim_timeout_secs), if claim_backoff == Duration::ZERO => {
                match res {
                    Ok(Some(claim)) => {
                        info!(
                            "领取 job run_id={} attempt={} workflow={:?} trust={}",
                            claim.run_id, claim.attempt, claim.workflow_path, claim.trust_level
                        );
                        claim_backoff = Duration::ZERO;
                        if let Err(e) = runner::execute(cfg.clone(), client.clone(), claim).await {
                            error!("job 执行失败：{e:#}");
                        }
                    }
                    Ok(None) => {
                        // 204：队列为空，继续长轮询。空轮询不算失败，复位退避。
                        claim_backoff = Duration::ZERO;
                    }
                    Err(e) => {
                        let wait = claim_backoff.max(Duration::from_secs(1));
                        warn!("claim 失败：{e:#}；{}s 后重试", wait.as_secs());
                        claim_backoff = wait;
                    }
                }
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    heartbeat.abort();
    info!("KychRunner 已停止");
    Ok(())
}
