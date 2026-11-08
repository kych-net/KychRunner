use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// 与主服务协商的契约版本。不兼容时主服务不会下发 job，执行器会记
/// `skipped / incompatible_version`，而不是静默跑错语义。
pub const PROTOCOL_VERSION: i32 = 1;

/// 唯一的快照来源标识。只做模式 B，心跳里固定上报 "api"。
pub const SNAPSHOT_SOURCE: &str = "api";

#[derive(Parser, Debug)]
#[command(
    name = "kychrunner",
    version,
    about = "KychNet 站内 Git 项目可选工作流执行器（模式 B）",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 用管理员提供的一次性注册码换取 runner token（明文只回显一次）
    Register {
        #[arg(long, env = "KYCHNET_URL", default_value = "https://kych.net")]
        url: String,
        #[arg(long)]
        code: String,
        #[arg(long, env = "RUNNER_NAME", default_value = "kychrunner")]
        name: String,
        /// 逗号分隔，例如 self-hosted,linux
        #[arg(long, env = "RUNNER_LABELS", default_value = "")]
        labels: String,
    },
    /// 长轮询领取 job 并执行。快照只经主服务 /api/runner/jobs/{id}/snapshot 下发
    Run {
        #[command(flatten)]
        cfg: Config,
    },
    /// 打印版本与协议版本
    Version,
    /// 环境自检（act / Docker / 目录权限），不连接主服务
    Doctor {
        #[command(flatten)]
        cfg: Config,
    },
}

#[derive(Args, Debug, Clone)]
pub struct Config {
    /// 主服务根地址，例如 https://kych.net
    #[arg(long, env = "KYCHNET_URL", default_value = "https://kych.net")]
    pub url: String,

    /// register 回显的 runner token。不要放命令行（会进 ps），优先用环境变量
    #[arg(long, env = "RUNNER_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    #[arg(long, env = "RUNNER_NAME", default_value = "kychrunner")]
    pub name: String,

    #[arg(long, env = "RUNNER_LABELS", default_value = "")]
    pub labels: String,

    /// nektos/act 可执行文件
    #[arg(long, env = "ACT_BIN", default_value = "act")]
    pub act_bin: PathBuf,

    /// 每次 run 的工作目录根（纯文件工作区，绝不创建 .git）
    #[arg(long, env = "WORK_ROOT", default_value = "/var/lib/kychrunner/runs")]
    pub work_root: PathBuf,

    /// 本地 action 目录，内含 noop-checkout/
    #[arg(long, env = "ACTIONS_DIR", default_value = "/opt/kychrunner/actions")]
    pub actions_dir: PathBuf,

    /// act 离线 action 缓存（--action-cache-path）
    #[arg(
        long,
        env = "ACT_CACHE",
        default_value = "/var/lib/kychrunner/act-cache"
    )]
    pub act_cache: PathBuf,

    /// -P ubuntu-latest=<image>，镜像须预先 build 在 runner 主机
    #[arg(long, env = "RUNNER_IMAGE", default_value = "kychnet/act-runner:1")]
    pub runner_image: String,

    #[arg(long, env = "HEARTBEAT_SECS", default_value_t = 60)]
    pub heartbeat_secs: u64,

    /// claim 长轮询等待秒数
    #[arg(long, env = "CLAIM_TIMEOUT_SECS", default_value_t = 30)]
    pub claim_timeout_secs: u64,

    #[arg(long, env = "MAX_SNAPSHOT_BYTES", default_value_t = 209_715_200)]
    pub max_snapshot_bytes: u64,

    #[arg(long, env = "MAX_LOG_BYTES", default_value_t = 8_388_608)]
    pub max_log_bytes: u64,

    /// 单次 job 的墙钟上限
    #[arg(long, env = "JOB_TIMEOUT_SECS", default_value_t = 900)]
    pub timeout_secs: u64,
}

impl Config {
    pub fn token(&self) -> Result<&str> {
        let t = self.token.as_deref().unwrap_or("").trim();
        if t.is_empty() {
            bail!(
                "缺少 runner token：先执行 `kychrunner register`，再设置 RUNNER_TOKEN 或 --token"
            );
        }
        Ok(t)
    }

    pub fn base_url(&self) -> String {
        self.url.trim_end_matches('/').to_string()
    }

    pub fn labels(&self) -> Vec<String> {
        parse_labels(&self.labels)
    }
}

pub fn parse_labels(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_trimmed_and_empty_dropped() {
        assert_eq!(
            parse_labels(" linux, ,self-hosted ,"),
            vec!["linux", "self-hosted"]
        );
        assert!(parse_labels("  ").is_empty());
    }

    #[test]
    fn base_url_has_no_trailing_slash() {
        let mut c = Config {
            url: "https://kych.net///".into(),
            token: Some("t".into()),
            name: "r".into(),
            labels: String::new(),
            act_bin: PathBuf::from("act"),
            work_root: PathBuf::from("/tmp/runs"),
            actions_dir: PathBuf::from("/opt/actions"),
            act_cache: PathBuf::from("/tmp/cache"),
            runner_image: "img".into(),
            heartbeat_secs: 60,
            claim_timeout_secs: 30,
            max_snapshot_bytes: 100,
            max_log_bytes: 100,
            timeout_secs: 900,
        };
        assert_eq!(c.base_url(), "https://kych.net");
        c.token = None;
        assert!(c.token().is_err());
    }
}
