//! claim 契约的数据结构。主服务与 KychRunner 只通过这里的 JSON + HTTP 交互。
//! 字段命名对齐 GitHub Checks：queued / in_progress / success / failure / skipped。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub fn default_workflow_dir() -> String {
    ".github/workflows".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Claim {
    pub run_id: String,
    pub attempt: i32,
    #[serde(default)]
    pub lease_until: Option<String>,
    pub project: Project,
    /// 代码快照对应的 commit（PR 事件为来源分支头）
    pub commit_oid: String,
    /// 定义快照，恒为默认分支 head —— 工作流 YAML 只从这里读
    pub def_commit_oid: String,
    #[serde(default)]
    pub ref_name: String,
    pub event: EventPayload,
    #[serde(default)]
    pub trust_level: String,
    /// PR / 非默认分支事件恒为空；主服务按 trust_level 强制，不信任 YAML 自声明
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// 主服务 dispatch 时按 project_workflows 指定；缺省时 runner 从定义快照里挑第一个
    #[serde(default)]
    pub workflow_path: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub pr_number: Option<i32>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub default_branch: String,
    #[serde(default = "default_workflow_dir")]
    pub workflow_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventPayload {
    /// push / pull_request / workflow_dispatch / schedule，直接传给 act 作为事件名
    pub name: String,
    /// GitHub 兼容 event.json 内容
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_snapshot_bytes")]
    pub max_snapshot_bytes: u64,
    #[serde(default = "default_log_bytes")]
    pub max_log_bytes: u64,
    #[serde(default = "default_network")]
    pub network: String,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout(),
            max_snapshot_bytes: default_snapshot_bytes(),
            max_log_bytes: default_log_bytes(),
            network: default_network(),
        }
    }
}

fn default_timeout() -> u64 {
    900
}
fn default_snapshot_bytes() -> u64 {
    209_715_200
}
fn default_log_bytes() -> u64 {
    8_388_608
}
fn default_network() -> String {
    "none".to_string()
}

// ---------------------------------------------------------------------------
// 回报体。所有请求都带 attempt，主服务用它做幂等校验：租约超时后旧 runner
// 的回报不会覆盖新 attempt 的结果。
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StartReport {
    pub attempt: i32,
    pub workflow_path: String,
    /// 预建 pending checks 用的 job 名；act 解析后再由 steps 补真实结果
    pub jobs: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LogChunk {
    pub attempt: i32,
    pub seq: u64,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub job_id: String,
    pub step_index: i32,
    pub name: String,
    pub status: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct StepsReport {
    pub attempt: i32,
    pub steps: Vec<StepReport>,
}

#[derive(Debug, Serialize)]
pub struct CompleteReport {
    pub attempt: i32,
    /// success | failure | cancelled
    pub conclusion: String,
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    pub runner_id: String,
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_tolerates_minimal_payload() {
        let json = r#"{
            "run_id":"r1","attempt":1,
            "project":{"id":"p1","slug":"demo","default_branch":"main"},
            "commit_oid":"1111","def_commit_oid":"2222",
            "event":{"name":"push","payload":{"ref":"refs/heads/main"}}
        }"#;
        let c: Claim = serde_json::from_str(json).unwrap();
        assert_eq!(c.project.workflow_dir, ".github/workflows");
        assert_eq!(c.trust_level, "");
        assert!(c.secrets.is_empty());
        assert_eq!(c.limits.network, "none");
        assert!(c.workflow_path.is_none());
        assert_eq!(c.limits.timeout_secs, 900);
    }
}
