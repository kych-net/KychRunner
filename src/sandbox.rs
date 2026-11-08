//! 每次 run 的目录布局与注入文件。
//! 环境变量走严格白名单：绝不把 DATABASE_URL / git token / 云凭据注入容器。

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::job::Claim;
use crate::snapshot;

#[derive(Debug, Clone)]
pub struct Layout {
    pub run_dir: PathBuf,
    pub workspace: PathBuf,
    pub defs_dir: PathBuf,
    pub snapshot_path: PathBuf,
    pub event_json: PathBuf,
    pub env_file: PathBuf,
    pub secret_file: PathBuf,
}

pub fn create_layout(root: &Path, run_id: &str) -> Result<Layout> {
    if !snapshot::valid_name(run_id) {
        bail!("非法 run_id（含路径分隔符或控制符）：{run_id:?}");
    }
    let run_dir = root.join(run_id);
    if run_dir.exists() {
        fs::remove_dir_all(&run_dir)
            .with_context(|| format!("清理旧的 {} 失败", run_dir.display()))?;
    }
    fs::create_dir_all(&run_dir).with_context(|| format!("创建 {} 失败", run_dir.display()))?;
    Ok(Layout {
        workspace: run_dir.join("workspace"),
        defs_dir: run_dir.join("definitions"),
        snapshot_path: run_dir.join("snapshot.tar.gz"),
        event_json: run_dir.join("event.json"),
        env_file: run_dir.join("env.allowlist"),
        secret_file: run_dir.join("secrets.env"),
        run_dir,
    })
}

pub fn cleanup(run_dir: &Path) -> Result<()> {
    if run_dir.exists() {
        fs::remove_dir_all(run_dir).with_context(|| format!("清理 {} 失败", run_dir.display()))?;
    }
    Ok(())
}

pub fn write_event_json(path: &Path, claim: &Claim) -> Result<()> {
    let mut payload = claim.event.payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "kychnet".to_string(),
            serde_json::json!({
                "project_id": claim.project.id,
                "project_slug": claim.project.slug,
                "run_id": claim.run_id,
                "commit_oid": claim.commit_oid,
                "def_commit_oid": claim.def_commit_oid,
                "ref": claim.ref_name,
                "trust_level": claim.trust_level,
                "pr_number": claim.pr_number,
                "snapshot_source": crate::config::SNAPSHOT_SOURCE
            }),
        );
    }
    fs::write(path, serde_json::to_vec_pretty(&payload)?)
        .with_context(|| format!("写 {} 失败", path.display()))
}

/// act 的 --env-file。只有这些变量会进入 job 容器。
pub fn write_env_allowlist(path: &Path, claim: &Claim) -> Result<()> {
    let mut f = File::create(path)?;
    let pairs: Vec<(&str, String)> = vec![
        ("KYCHNET_RUN_ID", claim.run_id.clone()),
        ("KYCHNET_PROJECT_ID", claim.project.id.clone()),
        ("KYCHNET_PROJECT_SLUG", claim.project.slug.clone()),
        ("KYCHNET_COMMIT_OID", claim.commit_oid.clone()),
        ("KYCHNET_DEF_COMMIT_OID", claim.def_commit_oid.clone()),
        ("KYCHNET_REF", claim.ref_name.clone()),
        ("KYCHNET_TRUST_LEVEL", claim.trust_level.clone()),
        (
            "KYCHNET_DEFAULT_BRANCH",
            claim.project.default_branch.clone(),
        ),
        ("KYCHNET_EVENT_NAME", claim.event.name.clone()),
        (
            "KYCHNET_PR_NUMBER",
            claim.pr_number.map(|n| n.to_string()).unwrap_or_default(),
        ),
    ];
    for (k, v) in pairs {
        if v.contains('\n') || v.contains('\r') {
            bail!("环境变量 {k} 含换行，拒绝注入");
        }
        writeln!(f, "{k}={v}")?;
    }
    Ok(())
}

/// act 的 --secret-file。GITHUB_TOKEN 显式置空，阻止 act 自动从 gh CLI 取 token。
/// 返回实际注入的 secrets，供日志打码使用。
pub fn write_secrets(path: &Path, claim: &Claim) -> Result<BTreeMap<String, String>> {
    let mut f = File::create(path)?;
    writeln!(f, "GITHUB_TOKEN=")?;
    let mut out = BTreeMap::new();
    if claim.trust_level == "pr" || claim.trust_level == "branch" {
        // 主服务已按 trust_level 强制清空；这里是第二道保险。
        return Ok(out);
    }
    for (k, v) in &claim.secrets {
        if k.contains('=') || k.contains('\n') || v.contains('\n') || v.contains('\r') {
            bail!("secret {k} 含非法字符，拒绝注入");
        }
        writeln!(f, "{k}={v}")?;
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

/// 把默认分支（def_commit_oid）的定义覆盖到 workspace 的 workflow_dir。
/// 先删掉工作区里该目录下的 yml/yaml，杜绝 PR 分支自带的工作流被 act 跑到。
pub fn install_definitions(defs_dir: &Path, workspace: &Path, workflow_dir: &str) -> Result<()> {
    let target = workspace.join(workflow_dir);
    fs::create_dir_all(&target).with_context(|| format!("创建 {} 失败", target.display()))?;
    for e in fs::read_dir(&target)? {
        let e = e?;
        if !e.file_type()?.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.ends_with(".yml") || name.ends_with(".yaml") {
            fs::remove_file(e.path())?;
        }
    }
    copy_tree(defs_dir, &target)?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for e in fs::read_dir(from).with_context(|| format!("读取 {} 失败", from.display()))? {
        let e = e?;
        let ft = e.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let src = e.path();
        let dst = to.join(e.file_name());
        if ft.is_dir() {
            fs::create_dir_all(&dst)?;
            copy_tree(&src, &dst)?;
        } else if ft.is_file() {
            fs::copy(&src, &dst)
                .with_context(|| format!("复制 {} -> {} 失败", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim() -> Claim {
        serde_json::from_str(
            r#"{
                "run_id":"r1","attempt":1,
                "project":{"id":"p1","slug":"demo","default_branch":"main"},
                "commit_oid":"1111","def_commit_oid":"2222","ref_name":"refs/heads/main",
                "trust_level":"pr",
                "event":{"name":"push","payload":{"ref":"refs/heads/main"}}
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn env_allowlist_has_no_db_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("env");
        write_env_allowlist(&p, &claim()).unwrap();
        let s = fs::read_to_string(&p).unwrap();
        assert!(s.contains("KYCHNET_COMMIT_OID=1111"));
        assert!(!s.contains("DATABASE_URL"));
        assert!(!s.contains("TOKEN"));
    }

    #[test]
    fn pr_secrets_are_forced_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("secrets");
        let mut c = claim();
        c.trust_level = "pr".into();
        c.secrets.insert("LEAK".into(), "should-not-appear".into());
        let injected = write_secrets(&p, &c).unwrap();
        assert!(injected.is_empty());
        let s = fs::read_to_string(&p).unwrap();
        assert_eq!(s.trim(), "GITHUB_TOKEN=");
        assert!(!s.contains("should-not-appear"));
    }

    #[test]
    fn definitions_replace_workspace_yml() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let wf = ws.join(".github/workflows");
        fs::create_dir_all(&wf).unwrap();
        fs::write(wf.join("evil.yml"), "from PR branch").unwrap();
        let defs = tmp.path().join("defs");
        fs::create_dir_all(&defs).unwrap();
        fs::write(defs.join("site.yml"), "from default branch").unwrap();

        install_definitions(&defs, &ws, ".github/workflows").unwrap();
        assert!(!wf.join("evil.yml").exists());
        assert_eq!(
            fs::read_to_string(wf.join("site.yml")).unwrap(),
            "from default branch"
        );
    }

    #[test]
    fn event_json_injects_kychnet_context() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("event.json");
        write_event_json(&p, &claim()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["kychnet"]["snapshot_source"], "api");
        assert_eq!(v["kychnet"]["commit_oid"], "1111");
    }
}
