//! 模式 B 快照解包：主服务把 PG 里的 Git 对象打包成 tar.gz 下发。
//!
//! tar 布局（见 docs/protocol.md）：
//!   code/<仓库内相对路径>      完整代码快照（commit_oid）
//!   definitions/<文件名>       默认分支 workflow_dir 下的 *.yml|*.yaml（def_commit_oid）
//!
//! 解包后工作区是**纯文件树，没有 .git 目录**，也不调用任何 git 命令。
//! 这里做纵深防御：即便主服务被绕过或打包出错，也不允许写出工作区之外。

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use tar::Archive;

pub const CODE_PREFIX: &str = "code/";
pub const DEF_PREFIX: &str = "definitions/";

#[derive(Debug, Clone)]
pub struct UnpackLimits {
    pub max_bytes: u64,
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Default for UnpackLimits {
    fn default() -> Self {
        Self {
            max_bytes: 209_715_200,
            max_entries: 50_000,
            max_depth: 32,
        }
    }
}

#[derive(Debug, Default)]
pub struct UnpackStats {
    pub code_files: usize,
    /// 已落盘到 defs_dir 的定义文件绝对路径
    pub definition_files: Vec<PathBuf>,
    pub bytes: u64,
}

/// 校验单个路径段。禁止 `.` / `..` / 分隔符 / NUL / 控制符 / `.git`。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.chars().any(char::is_control)
        && !name.eq_ignore_ascii_case(".git")
}

fn validate_relative(path: &Path, max_depth: usize) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_str().context("tar 条目路径含非 UTF-8 字节")?;
                if !valid_name(s) {
                    bail!("非法路径段 {:?}", s);
                }
                if s.eq_ignore_ascii_case(".git") {
                    bail!("拒绝在工作区创建 .git 目录");
                }
                parts.push(s.to_string());
            }
            // tar 里常见的 "./" 前缀，允许但丢弃。
            Component::CurDir => {}
            _ => bail!("拒绝绝对路径 / 父目录 / 根组件：{}", path.display()),
        }
    }
    if parts.is_empty() {
        bail!("空路径条目");
    }
    if parts.len() > max_depth {
        bail!("路径层级 {} 超过上限 {}", parts.len(), max_depth);
    }
    Ok(parts.join("/"))
}

fn ensure_within(dest: &Path, root: &Path) -> Result<()> {
    let root_c = fs::canonicalize(root).context("canonicalize 目标根失败")?;
    let parent = dest.parent().unwrap_or(root);
    let parent_c = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if !parent_c.starts_with(&root_c) {
        bail!("路径逃逸出目标目录：{}", dest.display());
    }
    Ok(())
}

/// 把一份 tar.gz 解包成 `workspace/`（代码）与 `defs_dir/`（定义）两棵树。
pub fn unpack(
    gz_path: &Path,
    workspace: &Path,
    defs_dir: &Path,
    limits: &UnpackLimits,
) -> Result<UnpackStats> {
    fs::create_dir_all(workspace).context("创建 workspace 失败")?;
    fs::create_dir_all(defs_dir).context("创建 definitions 目录失败")?;

    let file = File::open(gz_path).with_context(|| format!("打开 {} 失败", gz_path.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut stats = UnpackStats::default();
    let mut entries: usize = 0;

    for entry in archive.entries().context("读取 tar 条目失败")? {
        let mut entry = entry.context("tar 条目损坏")?;
        entries += 1;
        if entries > limits.max_entries {
            bail!("tar 条目数超过上限 {}", limits.max_entries);
        }

        let entry_type = entry.header().entry_type();
        // 只允许普通文件与目录；符号链接 / 硬链接 / 设备 / FIFO 一律拒绝。
        if !entry_type.is_file() && !entry_type.is_dir() {
            bail!(
                "拒绝非普通文件条目（{:?}）：{}",
                entry_type,
                entry
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
        }
        let size = entry.header().size().context("读取条目大小失败")?;
        let raw = entry.path().context("读取条目路径失败")?.to_path_buf();
        let rel = validate_relative(&raw, limits.max_depth)?;

        let (rest, target_root, is_code) = if let Some(r) = rel.strip_prefix(CODE_PREFIX) {
            (r.to_string(), workspace, true)
        } else if let Some(r) = rel.strip_prefix(DEF_PREFIX) {
            (r.to_string(), defs_dir, false)
        } else {
            bail!("未知快照前缀（只允许 code/ 与 definitions/）：{}", rel);
        };
        if rest.is_empty() {
            continue;
        }

        let dest = target_root.join(&rest);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建 {} 失败", parent.display()))?;
        }
        ensure_within(&dest, target_root)?;

        // 目标已存在且是符号链接时拒绝（防止覆盖到工作区外）。
        if let Ok(md) = fs::symlink_metadata(&dest) {
            if md.file_type().is_symlink() {
                bail!("目标已是符号链接：{}", dest.display());
            }
        }

        if entry_type.is_dir() {
            fs::create_dir_all(&dest)?;
            continue;
        }

        stats.bytes = stats.bytes.saturating_add(size);
        if stats.bytes > limits.max_bytes {
            bail!(
                "快照解压后 {} 字节超过限额 {} 字节",
                stats.bytes,
                limits.max_bytes
            );
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("解包 {} 失败", dest.display()))?;

        // 统一权限，避免 tar 里的 setuid / 可执行位带进容器工作区。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o644))?;
        }

        if is_code {
            stats.code_files += 1;
        } else {
            stats.definition_files.push(dest);
        }
    }

    Ok(stats)
}

/// 在定义目录里挑一个工作流文件，返回相对 defs_dir 的路径。
pub fn pick_definition(defs_dir: &Path) -> Result<String> {
    let mut found: Vec<String> = Vec::new();
    collect_yaml(defs_dir, defs_dir, &mut found)?;
    found.sort();
    found
        .into_iter()
        .next()
        .context("默认分支里没有找到 *.yml / *.yaml 工作流定义")
}

fn collect_yaml(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in fs::read_dir(dir).with_context(|| format!("读取 {} 失败", dir.display()))? {
        let e = e?;
        let p = e.path();
        let ft = e.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            collect_yaml(root, &p, out)?;
        } else if ft.is_file() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".yml") || name.ends_with(".yaml") {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn make_tar(dir: &Path, entries: Vec<(&str, &[u8], tar::EntryType)>) -> PathBuf {
        let gz = dir.join("snap.tar.gz");
        let f = File::create(&gz).unwrap();
        let enc = GzEncoder::new(f, Compression::default());
        let mut b = tar::Builder::new(enc);
        for (path, data, kind) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(kind);
            if kind == tar::EntryType::Symlink {
                h.set_link_name("../../etc/passwd").unwrap();
            }
            h.set_cksum();
            b.append_data(&mut h, path, data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        gz
    }

    #[test]
    fn valid_name_rejects_traversal_and_git_dir() {
        assert!(valid_name("src"));
        assert!(valid_name("site.yml"));
        assert!(!valid_name(".."));
        assert!(!valid_name("."));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a\\b"));
        assert!(!valid_name("a\0b"));
        assert!(!valid_name(".git"));
        assert!(!valid_name("a\u{1}b"));
    }

    #[test]
    fn unpacks_code_and_definitions_without_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let gz = make_tar(
            tmp.path(),
            vec![
                (
                    "code/README.md",
                    b"hello".as_slice(),
                    tar::EntryType::Regular,
                ),
                (
                    "code/.github/workflows/site.yml",
                    b"old".as_slice(),
                    tar::EntryType::Regular,
                ),
                (
                    "definitions/site.yml",
                    b"name: ci".as_slice(),
                    tar::EntryType::Regular,
                ),
            ],
        );
        let ws = tmp.path().join("workspace");
        let defs = tmp.path().join("defs");
        let stats = unpack(&gz, &ws, &defs, &UnpackLimits::default()).unwrap();
        assert_eq!(stats.code_files, 2);
        assert_eq!(stats.definition_files.len(), 1);
        assert_eq!(fs::read_to_string(ws.join("README.md")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(defs.join("site.yml")).unwrap(),
            "name: ci"
        );
        assert!(!ws.join(".git").exists());
        assert_eq!(pick_definition(&defs).unwrap(), "site.yml");
    }

    #[test]
    fn rejects_parent_traversal() {
        // tar crate 自身也拒绝含 `..` 的路径，所以这里直接验证我们的路径校验层。
        let err = validate_relative(Path::new("code/../../escape.txt"), 32).unwrap_err();
        assert!(err.to_string().contains("父目录"), "{err}");
        let err2 = validate_relative(Path::new("/etc/passwd"), 32).unwrap_err();
        assert!(err2.to_string().contains("父目录"), "{err2}");
    }

    #[test]
    fn rejects_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let gz = make_tar(
            tmp.path(),
            vec![("code/.git/config", b"x".as_slice(), tar::EntryType::Regular)],
        );
        let err = unpack(
            &gz,
            &tmp.path().join("ws"),
            &tmp.path().join("defs"),
            &UnpackLimits::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains(".git"), "{err}");
    }

    #[test]
    fn rejects_symlink_and_unknown_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let gz = make_tar(
            tmp.path(),
            vec![("code/link", b"".as_slice(), tar::EntryType::Symlink)],
        );
        let err = unpack(
            &gz,
            &tmp.path().join("ws"),
            &tmp.path().join("defs"),
            &UnpackLimits::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("非普通文件"), "{err}");

        let gz2 = make_tar(
            tmp.path(),
            vec![("other/x", b"x".as_slice(), tar::EntryType::Regular)],
        );
        let err2 = unpack(
            &gz2,
            &tmp.path().join("ws2"),
            &tmp.path().join("defs2"),
            &UnpackLimits::default(),
        )
        .unwrap_err();
        assert!(err2.to_string().contains("未知快照前缀"), "{err2}");
    }

    #[test]
    fn enforces_byte_and_entry_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let gz = make_tar(
            tmp.path(),
            vec![(
                "code/big.bin",
                b"0123456789".as_slice(),
                tar::EntryType::Regular,
            )],
        );
        let limits = UnpackLimits {
            max_bytes: 4,
            max_entries: 10,
            max_depth: 8,
        };
        let err = unpack(
            &gz,
            &tmp.path().join("ws"),
            &tmp.path().join("defs"),
            &limits,
        )
        .unwrap_err();
        assert!(err.to_string().contains("超过限额"), "{err}");
    }

    // 让 make_tar 的 finish() 返回值不被 unused_must_use 告警影响。
    #[allow(dead_code)]
    fn _write_trait_used<W: Write>(_: W) {}
}
