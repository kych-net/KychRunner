//! `kychrunner git ...`：在宿主机手工读写 `/git/<项目>/` 工作树。
//!
//! 主要给运维/调试用；job 内部的读写由 `runner.rs` 自动完成。工作流容器
//! 是 `--network=none`，step 不能直接调这些接口，回写走 `.kychnet/push.json`。

use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::PathBuf;

use crate::client::Client;
use crate::config::Config;

#[derive(Subcommand, Debug)]
pub enum GitAction {
    /// 列单层目录
    Tree {
        /// 主服务下发的 git_base，例如 /git/demo
        #[arg(long)]
        base: String,
        /// 分支名或 40 位 commit OID
        #[arg(long = "ref")]
        ref_name: String,
        /// 仓库内目录路径，空表示根目录
        #[arg(long, default_value = "")]
        path: String,
    },
    /// 读单个文件原始字节（自动还原 LFS pointer）
    File {
        #[arg(long)]
        base: String,
        #[arg(long = "ref")]
        ref_name: String,
        #[arg(long)]
        path: String,
        /// 不传则写到 stdout
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// 下载整棵工作树 tar.gz
    Archive {
        #[arg(long)]
        base: String,
        #[arg(long = "ref")]
        ref_name: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// 写文件并生成 commit
    Put {
        #[arg(long)]
        base: String,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        message: Option<String>,
    },
    /// 删除文件并生成 commit
    Delete {
        #[arg(long)]
        base: String,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        message: Option<String>,
    },
}

pub async fn run(cfg: &Config, action: GitAction) -> Result<()> {
    let client = Client::new(&cfg.base_url(), cfg.token()?)?;
    match action {
        GitAction::Tree {
            base,
            ref_name,
            path,
        } => {
            let items = client.git_tree(&base, &ref_name, &path).await?;
            for item in items {
                println!(
                    "{}\t{}",
                    if item.is_dir { "dir " } else { "file" },
                    item.path
                );
            }
        }
        GitAction::File {
            base,
            ref_name,
            path,
            output,
        } => {
            let data = client.git_file(&base, &ref_name, &path).await?;
            match output {
                Some(dest) => {
                    tokio::fs::write(&dest, &data)
                        .await
                        .with_context(|| format!("写入 {} 失败", dest.display()))?;
                    println!("已写入 {}（{} 字节）", dest.display(), data.len());
                }
                None => {
                    use std::io::Write;
                    std::io::stdout().write_all(&data)?;
                }
            }
        }
        GitAction::Archive {
            base,
            ref_name,
            output,
        } => {
            let meta = client
                .download_archive(&base, &ref_name, &output, cfg.max_snapshot_bytes)
                .await?;
            println!(
                "归档已写入 {}：commit={:?} files={:?} bytes={:?}",
                output.display(),
                meta.commit_oid,
                meta.files,
                meta.bytes
            );
        }
        GitAction::Put {
            base,
            branch,
            path,
            input,
            message,
        } => {
            let data = tokio::fs::read(&input)
                .await
                .with_context(|| format!("读取 {} 失败", input.display()))?;
            if data.len() as u64 > cfg.max_snapshot_bytes {
                anyhow::bail!("文件超过限额 {} 字节", cfg.max_snapshot_bytes);
            }
            client
                .git_put_file(&base, &branch, &path, &data, message.as_deref())
                .await?;
            println!("已写入 {branch}:{path}（{} 字节）", data.len());
        }
        GitAction::Delete {
            base,
            branch,
            path,
            message,
        } => {
            client
                .git_delete_file(&base, &branch, &path, message.as_deref())
                .await?;
            println!("已删除 {branch}:{path}");
        }
    }
    Ok(())
}
