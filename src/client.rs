//! 主服务 /api/runner/* HTTP 契约客户端。
//! 执行器只有 runner token，不接触任何数据库凭据；快照也只经这里下载。

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, StatusCode};
use serde::Serialize;
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
