//! 主服务 HTTP 契约客户端（protocol v2）。
//!
//! 执行器只有 runner token，不接触任何数据库凭据。v2 起 Git 数据直接经
//! `/git/<项目>/` 工作树接口读写；`/api/runner/*` 只负责调度与回报。
//! 同一个 Bearer token 复用到文件树写，服务端会把它绑定到当前 run。

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::AsyncWriteExt;

use crate::config::PROTOCOL_VERSION;
use crate::job::{
    Claim, CompleteReport, LogChunk, RegisterResponse, StartReport, StepReport, StepsReport,
};

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[derive(Debug, Default, Clone)]
pub struct SnapshotMeta {
    pub commit_oid: Option<String>,
    pub def_commit_oid: Option<String>,
    pub files: Option<u64>,
    pub bytes: Option<u64>,
}

/// `/git/<项目>/tree` 的单层目录条目。
#[derive(Debug, Clone, Deserialize)]
pub struct GitItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// `/git/<项目>/archive` 的响应头。
#[derive(Debug, Default, Clone)]
pub struct ArchiveMeta {
    pub commit_oid: Option<String>,
    pub files: Option<u64>,
    pub bytes: Option<u64>,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .user_agent(concat!("KychRunner/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("构造 HTTP 客户端失败")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header(header::AUTHORIZATION, format!("Bearer {}", self.token))
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.auth(self.http.get(self.url(path)))
    }

    fn post<T: Serialize + ?Sized>(&self, path: &str, body: &T) -> reqwest::RequestBuilder {
        self.auth(self.http.post(self.url(path))).json(body)
    }

    /// 注册：用一次性注册码换 token（明文只回显一次，之后服务端只存 hash）。
    pub async fn register(
        base: &str,
        name: &str,
        code: &str,
        labels: &[String],
    ) -> Result<RegisterResponse> {
        #[derive(Serialize)]
        struct Req<'a> {
            name: &'a str,
            code: &'a str,
            labels: &'a [String],
            protocol_version: i32,
            snapshot_source: &'a str,
        }
        let http = reqwest::Client::new();
        let resp = http
            .post(format!(
                "{}/api/runner/register",
                base.trim_end_matches('/')
            ))
            .json(&Req {
                name,
                code,
                labels,
                protocol_version: PROTOCOL_VERSION,
                snapshot_source: crate::config::SNAPSHOT_SOURCE,
            })
            .send()
            .await
            .context("注册请求失败")?;
        json_response(resp).await
    }

    /// 长轮询领 job；204 表示当前没有活。
    pub async fn claim(&self, labels: &[String], wait_secs: u64) -> Result<Option<Claim>> {
        let resp = self
            .get("/api/runner/jobs/claim")
            .query(&[
                ("protocol_version", PROTOCOL_VERSION.to_string()),
                ("wait", wait_secs.to_string()),
                ("labels", labels.join(",")),
            ])
            .send()
            .await
            .context("claim 请求失败")?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let claim: Claim = json_response(resp).await?;
        Ok(Some(claim))
    }

    /// 心跳：续租 + 上报在线状态 / act 版本 / 快照来源。
    pub async fn heartbeat(&self, act_version: &str, labels: &[String]) -> Result<()> {
        #[derive(Serialize)]
        struct Req<'a> {
            protocol_version: i32,
            snapshot_source: &'a str,
            act_version: &'a str,
            labels: &'a [String],
        }
        let resp = self
            .post(
                "/api/runner/heartbeat",
                &Req {
                    protocol_version: PROTOCOL_VERSION,
                    snapshot_source: crate::config::SNAPSHOT_SOURCE,
                    act_version,
                    labels,
                },
            )
            .send()
            .await
            .context("心跳请求失败")?;
        ok_response(resp).await
    }

    /// 模式 B 唯一取代码的方式：流式下载 tar.gz 到临时文件。
    /// 超过 max_bytes 立即中断，避免把整份快照堆进内存。
    pub async fn download_snapshot(
        &self,
        run_id: &str,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<SnapshotMeta> {
        let path = format!("/api/runner/jobs/{}/snapshot?kind=both", run_id);
        let resp = self.get(&path).send().await.context("快照请求失败")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("快照 HTTP {}: {}", status, truncate(&text));
        }
        let headers = resp.headers();
        let meta = SnapshotMeta {
            commit_oid: header_str(headers, "x-snapshot-commit"),
            def_commit_oid: header_str(headers, "x-snapshot-def-commit"),
            files: header_str(headers, "x-snapshot-files").and_then(|v| v.parse::<u64>().ok()),
            bytes: header_str(headers, "x-snapshot-bytes").and_then(|v| v.parse::<u64>().ok()),
        };

        if let Some(n) = resp.content_length() {
            if n > max_bytes {
                bail!("快照 {} 字节超过限额 {} 字节", n, max_bytes);
            }
        }

        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("创建 {} 失败", dest.display()))?;
        let mut stream = Box::pin(resp.bytes_stream());
        let mut total: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("读取快照流失败")?;
            total = total.saturating_add(chunk.len() as u64);
            if total > max_bytes {
                // 丢弃句柄后由调用方清理临时文件；此处明确报错，不落盘不完整快照。
                bail!("快照超过限额 {} 字节（已收 {} 字节）", max_bytes, total);
            }
            file.write_all(&chunk).await.context("写入快照文件失败")?;
        }
        file.flush().await.context("刷新快照文件失败")?;
        Ok(meta)
    }

    pub async fn start(&self, run_id: &str, body: &StartReport) -> Result<()> {
        ok_response(
            self.post(&format!("/api/runner/jobs/{}/start", run_id), body)
                .send()
                .await?,
        )
        .await
    }

    pub async fn logs(&self, run_id: &str, body: &LogChunk) -> Result<()> {
        ok_response(
            self.post(&format!("/api/runner/jobs/{}/logs", run_id), body)
                .send()
                .await?,
        )
        .await
    }

    pub async fn steps(&self, run_id: &str, steps: Vec<StepReport>, attempt: i32) -> Result<()> {
        let body = StepsReport { attempt, steps };
        ok_response(
            self.post(&format!("/api/runner/jobs/{}/steps", run_id), &body)
                .send()
                .await?,
        )
        .await
    }

    pub async fn complete(&self, run_id: &str, body: &CompleteReport) -> Result<()> {
        ok_response(
            self.post(&format!("/api/runner/jobs/{}/complete", run_id), body)
                .send()
                .await?,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // protocol v2：`/git/<项目>/` 工作树读写
    // -----------------------------------------------------------------------

    fn git_path(&self, git_base: &str, action: &str) -> String {
        format!("{}/{}", git_base.trim_end_matches('/'), action)
    }

    /// 单层目录列表。`ref` 用 40 位 commit OID，避免排队期间分支被顶掉。
    pub async fn git_tree(&self, git_base: &str, ref_oid: &str, dir: &str) -> Result<Vec<GitItem>> {
        #[derive(Deserialize)]
        struct Body {
            #[serde(default)]
            items: Vec<GitItem>,
        }
        let resp = self
            .auth(self.http.get(self.url(&self.git_path(git_base, "tree"))))
            .query(&[("ref", ref_oid), ("path", dir)])
            .send()
            .await
            .with_context(|| format!("读取工作树目录 {dir:?} 失败"))?;
        let body: Body = json_response(resp).await?;
        Ok(body.items)
    }

    /// 读单个文件原始字节；主服务已把 LFS pointer 还原成真实内容。
    pub async fn git_file(&self, git_base: &str, ref_oid: &str, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .auth(self.http.get(self.url(&self.git_path(git_base, "file"))))
            .query(&[("ref", ref_oid), ("path", path)])
            .send()
            .await
            .with_context(|| format!("读取工作树文件 {path:?} 失败"))?;
        bytes_response(resp).await
    }

    /// 整棵工作树 tar.gz，流式落盘；限额与 v1 快照一致。
    pub async fn download_archive(
        &self,
        git_base: &str,
        ref_oid: &str,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<ArchiveMeta> {
        let resp = self
            .auth(self.http.get(self.url(&self.git_path(git_base, "archive"))))
            .query(&[("ref", ref_oid)])
            .send()
            .await
            .context("工作树归档请求失败")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("归档 HTTP {}: {}", status, truncate(&text));
        }
        let headers = resp.headers();
        let meta = ArchiveMeta {
            commit_oid: header_str(headers, "x-git-commit"),
            files: header_str(headers, "x-git-files").and_then(|v| v.parse::<u64>().ok()),
            bytes: header_str(headers, "x-git-bytes").and_then(|v| v.parse::<u64>().ok()),
        };
        if let Some(n) = resp.content_length() {
            if n > max_bytes {
                bail!("归档 {} 字节超过限额 {} 字节", n, max_bytes);
            }
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("创建 {} 失败", dest.display()))?;
        let mut stream = Box::pin(resp.bytes_stream());
        let mut total: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("读取归档流失败")?;
            total = total.saturating_add(chunk.len() as u64);
            if total > max_bytes {
                bail!("归档超过限额 {} 字节（已收 {} 字节）", max_bytes, total);
            }
            file.write_all(&chunk).await.context("写入归档文件失败")?;
        }
        file.flush().await.context("刷新归档文件失败")?;
        Ok(meta)
    }

    /// 写文件并生成 commit。runner token 由服务端绑定到当前 run 与分支。
    pub async fn git_put_file(
        &self,
        git_base: &str,
        branch: &str,
        path: &str,
        data: &[u8],
        message: Option<&str>,
    ) -> Result<()> {
        let mut query = vec![("ref", branch), ("path", path)];
        if let Some(value) = message.filter(|v| !v.trim().is_empty()) {
            query.push(("message", value.trim()));
        }
        let resp = self
            .auth(self.http.put(self.url(&self.git_path(git_base, "file"))))
            .query(&query)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .with_context(|| format!("写入工作树文件 {path:?} 失败"))?;
        ok_response(resp).await
    }

    /// 删除文件并生成 commit。
    pub async fn git_delete_file(
        &self,
        git_base: &str,
        branch: &str,
        path: &str,
        message: Option<&str>,
    ) -> Result<()> {
        let mut query = vec![("ref", branch), ("path", path)];
        if let Some(value) = message.filter(|v| !v.trim().is_empty()) {
            query.push(("message", value.trim()));
        }
        let resp = self
            .auth(self.http.delete(self.url(&self.git_path(git_base, "file"))))
            .query(&query)
            .send()
            .await
            .with_context(|| format!("删除工作树文件 {path:?} 失败"))?;
        ok_response(resp).await
    }
}

async fn bytes_response(resp: reqwest::Response) -> Result<Vec<u8>> {
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("HTTP {}: {}", status, truncate(&text));
    }
    Ok(resp.bytes().await.context("读取响应体失败")?.to_vec())
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

async fn json_response<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("HTTP {}: {}", status, truncate(&text));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("解析 {} 响应 JSON 失败", status))
}

async fn ok_response(resp: reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    bail!("HTTP {}: {}", status, truncate(&text))
}
