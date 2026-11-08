# KychRunner

KychNet 站内 Git 项目的**可选工作流执行器**。独立 Rust 二进制、MIT 许可。
当前 HTTP 契约为 **protocol v2**。

> 主服务 `kych-net` 不依赖本程序：不安装、离线、临时禁用时，网站照常重建静态站；
> 仓库内 `.github/workflows/*.yml` 的工作流只是记为 `skipped / no_runner`。

## 只做模式 B

执行器**只有 runner token，零数据库凭据**。Git 数据一律经主服务 HTTP 接口读写，
v2 起直接走 `/git/<项目>/` 工作树接口，不再默认使用打包下发端点。

| | 模式 B / protocol v2（本仓库） |
| --- | --- |
| 执行器凭据 | **仅 runner token**，零数据库凭据、无 Git 令牌 |
| PG 暴露 | 完全不暴露，只有 HTTPS 到主服务 |
| 读代码 | `GET /git/<项目>/archive?ref=<commit_oid>` → tar.gz（仓库相对路径，无前缀） |
| 读定义 | `GET /git/<项目>/tree|file?ref=<def_commit_oid>`，恒取默认分支 head |
| 写回 | `PUT` / `DELETE /git/<项目>/file`（按 `.kychnet/push.json` 清单，显式回写） |
| LFS | 主服务读写时自动还原 / 生成 pointer，执行器无需挂载 `LFS_ROOT` |
| Git 仓库 | **不 clone、不 `git init`、不创建 `.git`、不调用 git CLI** |

因此 `Cargo.toml` 里没有 `sqlx`、没有数据库驱动、没有 `git2`；`kychrunner.service`
也没有任何 DSN 配置。

完整 HTTP 契约见 [`docs/protocol.md`](docs/protocol.md)。

## 职责

| 做 | 不做 |
| --- | --- |
| 长轮询领取 job、心跳续租、回报日志与结论 | 不写生产数据库（状态一律由主服务落库） |
| 把工作树归档铺成纯文件工作区 | 不解析 Git 对象、不读 PG、不 clone |
| 从默认分支单独拉取工作流定义并覆盖工作区 | 不执行 PR 分支自带的工作流 YAML |
| 按清单把产物回写到本次 run 绑定的分支 | 不自动 diff 工作区、不 push 任意 ref |
| 调 `nektos/act` 在断网 Docker 容器里跑 GitHub Actions 语法工作流 | 不实现自有 DSL、不实现 Actions 全语法 |
| 按 `trust_level` 隔离 PR 工作流（无 secrets、不得发布） | 不自行决定信任级别 |

## 工作流语义

- 定义默认在仓库 `.github/workflows/*.yml`，项目级 `workflow_dir` 可覆盖，对齐 GitHub Actions。
- **定义只从默认分支 head（`def_commit_oid`）读取**；PR 分支里改工作流不生效，新路径还需管理员启用。
- 代码工作树取事件对应的 `commit_oid`（PR 事件为来源分支头），按 40 位 OID 精确读取。
- 工作区里**没有 `.git`**：`actions/checkout@v4` 由 `--local-repository` 映射到
  `actions/noop-checkout`；工作流里不能用 `git log` / `git diff` / `git rev-parse`。
  提交信息改用注入的 `KYCHNET_COMMIT_OID`、`KYCHNET_REF`、`KYCHNET_PR_NUMBER`、
  `KYCHNET_TRUST_LEVEL` 和 `event.json` 里的 `kychnet` 节点。
- 在 GitHub Actions 里跑同一份工作流时 checkout 仍是真的 —— 这是唯一显著语义差异。
- 内置发布动作 `site.build` 由主服务独立排队执行，**不写进 YAML、不进容器**：
  `kych-net build` 以数据库为唯一事实源，容器内拿不到也不该拿到 `DATABASE_URL`。

### 显式回写产物

工作流容器 `--network=none`，step 不能直接调主服务。需要把生成的文件写回仓库时，
在工作区写一份 `.kychnet/push.json`：

```json
{
  "message": "chore: 生成产物",
  "files": ["dist/report.txt", "public/logo.png"],
  "delete": ["old.txt"]
}
```

- `ref` 省略时用 `claim.write_ref`；tag / 非分支事件没有可写分支，回写会失败。
- runner 只在 **act 成功**后回写；失败把 run 标为 `failure`。
- 每个路径生成一个 commit，作者记为 `kychrunner`；**不会自动 diff 工作区**，
  临时文件 / 缓存 / 依赖目录不会意外进仓库。
- 服务端把 runner token 绑定到「本次正在执行、租约有效、项目匹配」的 run，
  并只允许写该 run 绑定的分支；run 结束或执行器被禁用后写权限立即失效。

## CLI

```bash
# 1) 管理员在站内生成一次性注册码后，换 token（明文只回显一次）
kychrunner register --url https://kych.net --code <注册码> --name runner-1

# 2) 把 token 写进 /etc/kychrunner/env 的 RUNNER_TOKEN，然后启动
kychrunner run --url https://kych.net \
               --act-bin /usr/local/bin/act \
               --work-root /var/lib/kychrunner/runs

kychrunner version   # 版本 + 协议版本 + 数据通道
kychrunner doctor    # 环境自检：act / Docker / 目录权限，不连主服务

# 手工读写 /git/<项目>/ 工作树（运维 / 调试）
kychrunner git tree    --base /git/demo --ref main --path docs
kychrunner git file    --base /git/demo --ref main --path README.md --output /tmp/README.md
kychrunner git archive --base /git/demo --ref <40位commit> --output /tmp/tree.tar.gz
kychrunner git put     --base /git/demo --branch main --path public/a.txt --input ./a.txt
kychrunner git delete  --base /git/demo --branch main --path old.txt
```

所有参数都可用环境变量提供，见 [`.env.example`](.env.example)。
**不要把 token 放命令行**（会进 `ps`），优先 `RUNNER_TOKEN` 环境变量。

## 执行流程

```
claim（长轮询 30s，204 表示无活；protocol_version=2）
  → GET /git/<项目>/archive?ref=commit_oid（tar.gz 流式，限额 200MB）
  → GET /git/<项目>/tree + /file?ref=def_commit_oid（递归拉默认分支定义）
  → 解包归档到 workspace/，定义覆盖到 workspace/<workflow_dir>/
  → start（回报 workflow_path）
  → 生成 event.json / env.allowlist / secrets.env
  → act <event> -W <workflow_path> --action-offline-mode --network none --json
  → 流式解析 --json，分片回报 logs / steps
  → act 成功且存在 .kychnet/push.json → 按清单 PUT/DELETE 回写
  → complete（success | failure | cancelled）
  → 清理 run_dir
```

心跳每 60s 一次，服务端按 `executor_id` 续租全部 `in_progress` run；租约 5 分钟，
超时打回队列，`kill -9` 执行器也不会让 job 永久卡住。**租约同时是写权限的有效期**。

## 安全边界

| 威胁 | 措施 |
| --- | --- |
| push 权限升级为 RCE | 定义只从默认分支读；新路径需管理员启用；PR 工作流无 secrets、不得发布 |
| 执行器窃取/改写生产库 | 零 DB 凭据；写状态一律走 HTTP，由主服务落库 |
| 执行器越权写别的项目 / 分支 | 服务端校验 runner token 绑定的 in_progress run + project_id + 租约，且 `ref` 必须等于该 run 的分支 |
| 快照/归档路径穿越 | 解包时拒绝 `..`、绝对路径、`.git`、符号链接、非普通文件；`canonicalize` 复检；字节/条目/深度限额 |
| 回写把缓存/临时文件提交进仓库 | 只按显式清单回写，不 diff 工作区；路径逐段校验、单文件与总量限额、拒绝符号链接 |
| 容器逃逸 | 禁 Docker socket、断网、`--cap-drop=ALL`、`no-new-privileges`、CPU/内存/pids 限额 |
| 容器读到 `.env` / 凭据 | 显式 `--env-file` 白名单；只注入 `KYCHNET_GIT_BASE` / `KYCHNET_WRITE_REF` / `KYCHNET_PUSH_MANIFEST` 等非敏感值；`GITHUB_TOKEN=` 置空 |
| 日志泄露 secrets | 按 `secrets.env` 的键值二次打码后再回报 |
| 许可证污染 | KychRunner(MIT) 不复制 KychNet(AGPL-3.0) 的 `src/git/*` 源码；两仓只通过 HTTP 契约交互 |

## 升级注意

protocol v2 是**不兼容升级**：主服务升到 v2 后，v1 执行器的 `register` / `claim`
会被 400 拒绝。升级时先部署主服务，再重启 KychRunner；`kychrunner version`
可确认协议版本。v1 的 `/api/runner/jobs/{id}/snapshot` 端点仍保留，
但只在 claim 未下发 `git_base` 时作为兼容回退使用。

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
