//! 显式回写（protocol v2）。
//!
//! 工作流容器是 `--network=none`，step 不能自己调用主服务。因此约定：工作流
//! 在工作区写一份清单 `.kychnet/push.json`，列出要回写的路径；runner 宿主进程
//! 在 act 成功后据此调用 `/git/<项目>/file` 的 PUT / DELETE。
//!
//! **不自动 diff 工作区**：只有清单里显式列出的路径才会被回写。这样工作流
//! 生成的临时文件、缓存、依赖目录都不会意外进仓库。

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::client::Client;
use crate::job::Claim;
use crate::snapshot;

pub const MANIFEST_REL: &str = ".kychnet/push.json";
pub const MAX_FILES: usize = 256;
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// 清单格式（写在工作区根下）：
///
/// ```json
/// { "ref": "main", "message": "chore: 生成产物",
///   "files": ["dist/report.txt", "public/logo.png"],
///   "delete": ["old.txt"] }
/// ```
///
/// `ref` 省略时用 claim 下发的 `write_ref`；tag / 非分支事件没有 write_ref，
/// 此时回写会直接失败。`files` 里的路径相对工作区，内容作为请求体发送。
#[derive(Debug, Deserialize)]
pub struct PushManifest {
    #[serde(default, rename = "ref")]
    pub ref_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub delete: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PushOutcome {
    pub written: usize,
    pub deleted: usize,
    pub bytes: u64,
}

/// 校验单个回写路径：必须是仓库内相对路径，每段都通过 `valid_name()`。
fn valid_path(value: &str) -> bool {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') {
        return false;
    }
    let parts: Vec<&str> = value.split('/').collect();
    parts.len() <= 32 && parts.iter().all(|p| snapshot::valid_name(p))
}

fn validate(m: &PushManifest) -> Result<()> {
    if m.files.len() + m.delete.len() > MAX_FILES {
        bail!("回写路径数超过上限 {}", MAX_FILES);
    }
    if m.files.is_empty() && m.delete.is_empty() {
        bail!("回写清单里没有任何路径");
    }
    if let Some(message) = &m.message {
        if message.contains('\n') || message.contains('\r') || message.len() > 200 {
            bail!("回写 message 含换行或超过 200 字节");
        }
    }
    let mut seen = BTreeSet::new();
    for p in m.files.iter().chain(m.delete.iter()) {
        if !valid_path(p) {
            bail!("非法回写路径 {p:?}");
        }
        if !seen.insert(p.as_str()) {
            bail!("回写路径重复：{p}");
        }
    }
    Ok(())
}

/// 读取并校验清单。文件不存在是正常情况（不回写），返回 None。
pub fn load(workspace: &Path) -> Result<Option<PushManifest>> {
    let path = workspace.join(MANIFEST_REL);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(md) => md,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("检查 {} 失败", path.display())),
    };
    if md.file_type().is_symlink() {
        bail!("回写清单不能是符号链接");
    }
    if !md.is_file() {
        bail!("回写清单不是普通文件");
    }
    if md.len() > MAX_MANIFEST_BYTES {
        bail!("回写清单超过 {} 字节", MAX_MANIFEST_BYTES);
    }
    let data = std::fs::read(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let manifest: PushManifest = serde_json::from_slice(&data).context("解析回写清单 JSON 失败")?;
    validate(&manifest)?;
    Ok(Some(manifest))
}

fn safe_join(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        out.push(part);
    }
    out
}

/// 按清单回写。任一文件失败即整体报错，由调用方把 run 标为 failure。
pub async fn apply(client: &Client, claim: &Claim, workspace: &Path) -> Result<PushOutcome> {
    let Some(manifest) = load(workspace)? else {
        return Ok(PushOutcome::default());
    };
    let git_base = claim
        .git_base
        .as_deref()
        .context("当前协议不支持 /git/ 回写（claim 缺少 git_base）")?;
    let branch = manifest
        .ref_name
        .as_deref()
        .or(claim.write_ref.as_deref())
        .filter(|b| !b.is_empty())
        .context("该事件没有可回写的分支（tag / 非 refs/heads）")?;

    let default_message = format!("KychRunner run {} 产物回写", claim.run_id);
    let message = manifest
        .message
        .as_deref()
        .unwrap_or(default_message.as_str());

    let root_c = tokio::fs::canonicalize(workspace)
        .await
        .context("canonicalize 工作区失败")?;
    let mut out = PushOutcome::default();

    for p in &manifest.files {
        let src = safe_join(workspace, p);
        let parent = src.parent().unwrap_or(workspace);
        let parent_c = tokio::fs::canonicalize(parent)
            .await
            .with_context(|| format!("回写文件的父目录不存在：{p}"))?;
        if !parent_c.starts_with(&root_c) {
            bail!("回写路径逃逸出工作区：{p}");
        }
        let md = tokio::fs::symlink_metadata(&src)
            .await
            .with_context(|| format!("回写文件不存在：{p}"))?;
        if md.file_type().is_symlink() || !md.is_file() {
            bail!("回写目标不是普通文件：{p}");
        }
        if md.len() > MAX_FILE_BYTES {
            bail!("回写文件 {p} 超过单文件限额 {} 字节", MAX_FILE_BYTES);
        }
        out.bytes = out.bytes.saturating_add(md.len());
        if out.bytes > MAX_TOTAL_BYTES {
            bail!("回写总字节数超过限额 {}", MAX_TOTAL_BYTES);
        }
        let data = tokio::fs::read(&src)
            .await
            .with_context(|| format!("读取 {p} 失败"))?;
        client
            .git_put_file(git_base, branch, p, &data, Some(message))
            .await
            .with_context(|| format!("回写 {p} 失败"))?;
        out.written += 1;
    }

    for p in &manifest.delete {
        client
            .git_delete_file(git_base, branch, p, Some(message))
            .await
            .with_context(|| format!("删除 {p} 失败"))?;
        out.deleted += 1;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_path_rejects_traversal_and_git_dir() {
        assert!(valid_path("dist/report.txt"));
        assert!(valid_path(".github/workflows/site.yml"));
        assert!(!valid_path(""));
        assert!(!valid_path("/etc/passwd"));
        assert!(!valid_path("a/../b"));
        assert!(!valid_path(".git/config"));
        assert!(!valid_path("a\\b"));
    }

    #[test]
    fn manifest_requires_paths_and_unique_entries() {
        let empty: PushManifest = serde_json::from_str(r#"{}"#).unwrap();
        assert!(validate(&empty).is_err());

        let dup: PushManifest =
            serde_json::from_str(r#"{"files":["a.txt"],"delete":["a.txt"]}"#).unwrap();
        assert!(validate(&dup).is_err());

        let ok: PushManifest = serde_json::from_str(
            r#"{"ref":"main","message":"chore: 产物","files":["dist/a.txt"],"delete":["old.txt"]}"#,
        )
        .unwrap();
        assert!(validate(&ok).is_ok());
        assert_eq!(ok.ref_name.as_deref(), Some("main"));
    }
}
