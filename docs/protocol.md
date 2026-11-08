# KychRunner ↔ kych-net HTTP 契约（模式 B / protocol_version = 2）

本文件是**两个仓库之间唯一的接口约定**。KychRunner（MIT）只依赖这里描述的 HTTP 行为，
不依赖 KychNet 的内部表结构或源码；主服务实现见 KychNet 仓库的
`src/workflow/runner_api.rs` 与 `src/api.rs`。

> **v2 是不兼容升级**：执行器不再默认走 `/api/runner/jobs/{id}/snapshot`，
> 改为直接读写 `/git/<项目>/` 工作树。主服务与 KychRunner 必须同步升级，
> 否则 v1 执行器的 `claim` 会被 `400` 拒绝（不会静默跑错语义）。

## 1. 认证

所有 `/api/runner/*` 请求（除 `register`）以及 `/git/<项目>/file` 的写请求带：

```
Authorization: Bearer <runner token>
```

服务端存 `workflow_runners.token_hash`（SHA-256），明文只在注册时回显一次。
`enabled=false` 或撤销 token 后，所有请求（含已在执行的 run）立即 403 —— 这就是「临时禁用」。

`/git/<项目>/{tree,file,archive}` 的**读**沿用站内 Git 的公开只读语义，带不带 token 都可读
（packfile 本来就含全部内容，工作树接口没有放大暴露面）。

所有回报请求都带 `run_id`（路径）+ `attempt`（body）。服务端做幂等校验：租约超时后
旧 runner 的回报不能覆盖新 attempt 的结果。

## 2. 端点

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/api/runner/register` | 用一次性注册码换 `{runner_id, token}` |
| `GET` | `/api/runner/jobs/claim` | 长轮询领 job；无 job 返回 **204** |
| `GET` | `/api/runner/jobs/{id}/snapshot?kind=both` | **v1 兼容端点**，v2 不再默认使用 |
| `POST` | `/api/runner/jobs/{id}/start` | 回报工作流路径，预建 pending checks |
| `POST` | `/api/runner/jobs/{id}/logs` | 追加日志分片 |
| `POST` | `/api/runner/jobs/{id}/steps` | 批量回报 job/step 状态 |
| `POST` | `/api/runner/jobs/{id}/complete` | 结论 + 退出码 |
| `POST` | `/api/runner/heartbeat` | 续租 + 在线状态 |
| `GET` | `/git/<项目>/tree?ref=&path=` | **v2** 单层目录 JSON |
| `GET` | `/git/<项目>/file?ref=&path=` | **v2** 文件原始字节（还原 LFS pointer） |
| `GET` | `/git/<项目>/archive?ref=` | **v2** 整棵工作树 tar.gz（仓库相对路径，无前缀） |
| `PUT` | `/git/<项目>/file?ref=&path=&message=` | **v2** 写文件并生成 commit |
| `DELETE` | `/git/<项目>/file?ref=&path=&message=` | **v2** 删文件并生成 commit |

> `<项目>` 是项目 slug（不带 `.git`）。带 `.git` 的路径是 smart HTTP 通道
> （`info/refs` / `upload-pack` / `receive-pack` / LFS），KychRunner **不使用**。

### register

```json
// 请求
{ "name": "runner-1", "code": "<一次性注册码>", "labels": ["self-hosted","linux"],
  "protocol_version": 2, "snapshot_source": "api" }
// 200
{ "runner_id": "…", "token": "…" }
```

`snapshot_source` 是历史字段名，含义是「只经主服务 HTTP 接口取 Git 数据、绝不直连
PostgreSQL」。v2 的具体通道是 `/git/`，但仍不是 PG 直连，故**值保持 `api`**，
服务端也只接受这个值。

### claim

`GET /api/runner/jobs/claim?protocol_version=2&wait=30&labels=self-hosted,linux`

- 无 job：`204 No Content`（执行器继续长轮询，不算错误）。
- 有 job：`200` + 下面 JSON。

```json
{
  "run_id": "…",
  "attempt": 1,
  "lease_until": "2026-11-08T09:40:00+08:00",
  "project": { "id": "…", "slug": "demo", "default_branch": "main",
               "workflow_dir": ".github/workflows" },
  "commit_oid": "1111111111111111111111111111111111111111",
  "def_commit_oid": "2222222222222222222222222222222222222222",
  "ref_name": "refs/heads/main",
  "git_base": "/git/demo",
  "write_ref": "main",
  "workflow_path": ".github/workflows/site.yml",
  "actor": "kych",
  "pr_number": null,
  "trust_level": "default",
  "secrets": {},
  "event": { "name": "push", "payload": { "…": "GitHub 兼容 event.json" } },
  "limits": { "timeout_secs": 900, "max_snapshot_bytes": 209715200,
              "max_log_bytes": 8388608, "network": "none" }
}
```

关键语义：

- `commit_oid` = **代码工作树**（事件对应 commit；PR 事件为来源分支头）。
- `def_commit_oid` = **定义来源**，恒为默认分支 head。工作流 YAML 只从这里读，
  PR 分支里改工作流不生效。
- `git_base` = `/git/<slug>`，执行器据此拼出 `tree` / `file` / `archive` 路径。
- `write_ref` = 本次 run 允许回写的分支名；tag / 非 `refs/heads/` 事件为 `null`，
  此时禁止回写。
- `trust_level` 由主服务强制：`pr` / `branch` 时 `secrets` 必须为空且不得发布；
  `default` / `manual` 才可能有 secrets。执行器会二次校验，不信任 YAML 自声明。
- **没有 `repo_url`、没有 `git_token`、没有 `pg_dsn`** —— 执行器只持 runner token，
  不 clone、不 `git init`、不生成 `.git`。

若 claim 未带 `git_base`（兼容旧主服务），执行器回退到 v1 的
`/api/runner/jobs/{id}/snapshot` 并在日志里告警，不静默切换语义。

## 3. v2 Git 数据通道

### 读

执行器按 `claim` 下发的 **40 位 commit OID** 精确读取，不用分支名 —— 这样 run 排队期间
分支被新 push 顶掉也不会读错内容。

1. `GET {git_base}/archive?ref={commit_oid}` → tar.gz，条目路径直接是仓库相对路径
   （**没有 `code/` 前缀**），全部解到 `workspace/`。响应头：
   `X-Git-Commit` / `X-Git-Files` / `X-Git-Bytes`。超过 `max_snapshot_bytes`
   （默认 200 MiB）服务端返回 413，执行器流式下载时二次限额。
2. `GET {git_base}/tree?ref={def_commit_oid}&path={workflow_dir}` → 单层目录 JSON
   `{ "commit": "…", "path": "…", "items": [{ "name", "path", "is_dir", … }] }`；
   递归子目录，对 `*.yml` / `*.yaml` 逐个
   `GET {git_base}/file?ref={def_commit_oid}&path=…` 下载到 `definitions/`。
3. `sandbox::install_definitions()` 先删掉工作区 `workflow_dir` 下的 yml/yaml，
   再把默认分支定义覆盖进去，杜绝 PR 分支自带的工作流被 act 跑到。

工作区始终是**纯文件树，没有 `.git`**；`actions/checkout@v4` 映射为本地 no-op action。

### 写（显式回写）

工作流容器是 `--network=none`，step **不能直接调用主服务**。约定由工作流在工作区写一份
清单 `.kychnet/push.json`：

```json
{
  "ref": "main",
  "message": "chore: 生成产物",
  "files": ["dist/report.txt", "public/logo.png"],
  "delete": ["old.txt"]
}
```

- `ref` 省略时用 `claim.write_ref`；tag / 非分支事件没有 write_ref，回写直接失败。
- `files` 里的路径相对工作区，文件内容作为 `PUT` 请求体发送（二进制由主服务走 LFS）。
- **不自动 diff 工作区**：只有清单里显式列出的路径才会提交，临时文件 / 缓存 / 依赖
  目录不会意外进仓库。
- 每个路径生成一个 commit，`message` 相同；作者记为 `kychrunner`，
  `git_commits.author_id` 为 NULL（runner 不是站内用户）。
- 限额：最多 256 个路径、单文件 32 MiB、总量 64 MiB、清单 64 KiB；路径逐段校验，
  拒绝 `..` / 绝对路径 / `.git` / 符号链接 / 控制符。
- runner 只在 **act 成功**时执行回写；回写失败把 run 标为 `failure` 并在 `complete.error`
  里说明，不会因为部分成功而误报 success。

手工读写可用 `kychrunner git tree|file|archive|put|delete`（宿主机上执行）。

### 服务端写鉴权（主服务侧）

`/git/<项目>/file` 的 `PUT` / `DELETE` 认证顺序：

1. Git 令牌（`git_tokens`）或登录会话 → 可写任意分支（管理端语义不变）。
2. **runner token** → 仅当满足全部条件才放行，且只能写本次 run 绑定的分支：
   - `workflow_runners.token_hash` 命中且 `enabled=true`；
   - 存在 `workflow_runs`：`executor_id` 归属该 runner、`project_id` 为当前项目、
     `status='in_progress'`、`lease_until > now()`；
   - 请求 `ref` 等于该 run 的 `refs/heads/<branch>`，否则 403。

   run 结束 / 租约过期 / 执行器被禁用后，写权限立即失效，无需轮换凭据。

3. 其余情况 401。

> smart HTTP 的 `git-receive-pack` / LFS 上传**不认 runner token**，仍只用 Git 令牌或
> 登录会话：runner 只需要写工作树文件，不需要 push 任意 ref。

## 4. start / logs / steps / complete

```json
// POST start
{ "attempt": 1, "workflow_path": ".github/workflows/site.yml", "jobs": [] }

// POST logs（可多次，seq 递增）
{ "attempt": 1, "seq": 1, "lines": ["[build] cargo test", "…"] }

// POST steps（可多次）
{ "attempt": 1, "steps": [
  { "job_id": "build", "step_index": 0, "name": "Checkout",
    "status": "success", "exit_code": 0 }
] }

// POST complete
{ "attempt": 1, "conclusion": "success", "exit_code": 0, "error": null }
```

`conclusion` 取值对齐 GitHub Checks：`success` / `failure` / `cancelled`。
`steps.status`：`pending` / `in_progress` / `success` / `failure` / `skipped`。

服务端把 `logs` 追加到 `workflow_runs.log_path` 指向的文件（NDJSON 或纯文本），
DB 里只存路径，不把日志塞进 PostgreSQL。

### heartbeat

```json
{ "protocol_version": 2, "snapshot_source": "api",
  "act_version": "act version 0.2.xx", "labels": ["self-hosted"] }
```

服务端更新 `last_seen_at`，并把该 executor 名下所有 `in_progress` run 的
`lease_until` 续到 `now() + 5min`。心跳间隔 60s。**租约同时是写权限的有效期**。

## 5. 状态机与降级

| 状态 | 触发 | 行为 |
| --- | --- | --- |
| 未安装 | 无存活 runner | 新 run 直接 `skipped / no_runner`；不阻塞 push；内置 `site.build` 照常 |
| 临时禁用 | `enabled=false` / 撤销 token | 同上；已 `queued` run 保留到 `expires_at`，恢复后可重放 |
| 安装并启用 | 存活且协议兼容 | `queued → claimed → in_progress`（带租约） |
| 协议不兼容 | v1 执行器 / 旧主服务 | claim 400；执行器日志明确报版本不符 |
| 心跳超时 | `lease_until < now()` | 打回 `queued`，`attempt+1`；超过 3 次标 `failure / runner_lost` |
| 归档不可用 | 对象被 gc / 主服务忙 | `failure / snapshot_unavailable`，**不影响内置动作** |
| 回写失败 | 清单非法 / 越权 / 分支被顶 | `failure`，`error` 带具体原因；已生成的 commit 不回滚 |

UI 必须区分 `skipped / no_runner`、`skipped / disabled`、`expired`、`failure` —— 否则
「没装执行器」会被误读成工作流失败。

**执行器缺席时内置动作照跑**：`site.build` / `build_page` 由主服务 `BuildQueue` 独立排队，
与 KychRunner 是否存在、YAML 工作流是否成功都无关。可选 `publish_on_check_success`
（默认 false）才让发布等待必需 checks 全绿。

## 6. 明确不做

- 不提供 `pg_dsn` / 只读视图 / 直连 PostgreSQL 通道（只做模式 B）。
- 不 clone、不 `git init`、不生成 `.git`、不在工作流里调用 git。
- 不让 runner token 用于 smart HTTP push 或 LFS 上传。
- 不自动把工作区 diff 回写仓库，必须显式列清单。
- 不在主服务进程内执行任何工作流步骤，主服务不依赖 Docker 或 act。
- 不接入 `actions/runner` / GitLab Runner / Gitea `act_runner`（都是平台 agent，不是独立执行器）。
