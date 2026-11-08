# KychRunner

KychNet 站内 Git 项目的**可选工作流执行器**。独立 Rust 二进制、MIT 许可。

> 主服务 `kych-net` 不依赖本程序：不安装、离线、临时禁用时，网站照常重建静态站；
> 仓库内 `.github/workflows/*.yml` 的工作流只是记为 `skipped / no_runner`。

## 只做模式 B

本实现**只有一种快照来源**：经主服务 `kych-net` 的 HTTP 接口读取 PostgreSQL 里的 Git 仓库。

| | 模式 B（本仓库） |
| --- | --- |
| 执行器凭据 | **仅 runner token**，零数据库凭据 |
| PG 暴露 | 完全不暴露，只有 HTTPS 到主服务 |
| 快照 | `GET /api/runner/jobs/{id}/snapshot?kind=both` 返回 tar.gz 流式包 |
| LFS | 主服务打包时把 pointer 还原成真实字节，执行器无需挂载 `LFS_ROOT` |
| Git 仓库 | **不 clone、不 `git init`、不创建 `.git`、不调用 git CLI** |

因此 `Cargo.toml` 里没有 `sqlx`、没有数据库驱动、没有 `git2`；`kychrunner.service`
也没有任何 DSN 配置。执行器只做四件事：领 job、下载并解包快照、调 `nektos/act`、回报状态与日志。

完整 HTTP 契约见 [`docs/protocol.md`](docs/protocol.md)。

## 职责

| 做 | 不做 |
| --- | --- |
| 长轮询领取 job、心跳续租、回报日志与结论 | 不写生产数据库（状态一律由主服务落库） |
| 把 tar.gz 快照铺成纯文件工作区 | 不解析 Git 对象、不读 PG、不 clone |
| 调 `nektos/act` 在断网 Docker 容器里跑 GitHub Actions 语法工作流 | 不实现自有 DSL、不实现 Actions 全语法 |
| 按 `trust_level` 隔离 PR 工作流（无 secrets、不得发布） | 不自行决定信任级别 |

## 工作流语义

- 定义默认在仓库 `.github/workflows/*.yml`，项目级 `workflow_dir` 可覆盖，对齐 GitHub Actions。
- **定义只从默认分支快照（`def_commit_oid`）读取**；PR 分支里改工作流不生效，新路径还需管理员启用。
- 代码快照取事件对应的 `commit_oid`（PR 事件为来源分支头）。
- 工作区里**没有 `.git`**：`actions/checkout@v4` 由 `--local-repository` 映射到
  `actions/noop-checkout`；工作流里不能用 `git log` / `git diff` / `git rev-parse`。
  提交信息改用注入的 `KYCHNET_COMMIT_OID`、`KYCHNET_REF`、`KYCHNET_PR_NUMBER`、
  `KYCHNET_TRUST_LEVEL` 和 `event.json` 里的 `kychnet` 节点。
- 在 GitHub Actions 里跑同一份工作流时 checkout 仍是真的 —— 这是唯一显著语义差异。
- 内置发布动作 `site.build` 由主服务独立排队执行，**不写进 YAML、不进容器**：
  `kych-net build` 以数据库为唯一事实源，容器内拿不到也不该拿到 `DATABASE_URL`。

## CLI

```bash
# 1) 管理员在站内生成一次性注册码后，换 token（明文只回显一次）
kychrunner register --url https://kych.net --code <注册码> --name runner-1

# 2) 把 token 写进 /etc/kychrunner/env 的 RUNNER_TOKEN，然后启动
kychrunner run --url https://kych.net \
               --act-bin /usr/local/bin/act \
               --work-root /var/lib/kychrunner/runs

kychrunner version   # 版本 + 协议版本 + 快照来源
kychrunner doctor    # 环境自检：act / Docker / 目录权限，不连主服务
```

所有参数都可用环境变量提供，见 [`.env.example`](.env.example)。
**不要把 token 放命令行**（会进 `ps`），优先 `RUNNER_TOKEN` 环境变量。

## 执行流程

```
claim（长轮询 30s，204 表示无活）
  → GET snapshot?kind=both（tar.gz 流式，限额 200MB）
  → 解包：code/ → workspace/，definitions/ → workspace/<workflow_dir>/
  → start（回报 workflow_path）
  → 生成 event.json / env.allowlist / secrets.env
  → act <event> -W <workflow_path> --action-offline-mode --network none --json
  → 流式解析 --json，分片回报 logs / steps
  → complete（success | failure | cancelled）
  → 清理 run_dir
```

心跳每 60s 一次，服务端按 `executor_id` 续租全部 `in_progress` run；租约 5 分钟，
超时打回队列，`kill -9` 执行器也不会让 job 永久卡住。

## 安全边界

| 威胁 | 措施 |
| --- | --- |
| push 权限升级为 RCE | 定义只从默认分支读；新路径需管理员启用；PR 工作流无 secrets、不得发布 |
| 执行器窃取/改写生产库 | 零 DB 凭据；写状态一律走 HTTP，由主服务落库 |
| 快照接口越权 | 主服务校验 token + `executor_id` 归属 + `in_progress` + 租约有效 + commit 属于该 run |
| 恶意快照 / 路径穿越 | 解包时拒绝 `..`、绝对路径、`.git`、符号链接、非普通文件；`canonicalize` 复检；字节/条目/深度限额 |
| 容器逃逸 | 禁 Docker socket、断网、`--cap-drop=ALL`、`no-new-privileges`、CPU/内存/pids 限额 |
| 容器读到 `.env` / 凭据 | 显式 `--env-file` 白名单；`GITHUB_TOKEN=` 置空；工作区只放快照文件 |
| 日志泄露 secrets | 按 `secrets.env` 的键值二次打码后再回报 |
| 许可证污染 | KychRunner(MIT) 不复制 KychNet(AGPL-3.0) 的 `src/git/*` 源码；两仓只通过 HTTP 契约交互 |

## 部署

依赖：Docker Engine、预置 `act` 二进制、`kychnet/act-runner:1` 镜像（离线模式不会 pull）、
`ACT_CACHE` 里预先缓存的 action。systemd unit 见 [`kychrunner.service`](kychrunner.service)。

`--action-offline-mode` 下 `uses:` 只允许离线缓存白名单，缓存缺失直接失败而不联网。
依赖 GitHub API 的 marketplace action（OIDC、artifact、cache）在断网容器内不可用。

## 开发

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 许可

MIT。
