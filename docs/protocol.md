# KychRunner ↔ kych-net HTTP 契约（模式 B / protocol_version = 1）

本文件是**两个仓库之间唯一的接口约定**。KychRunner（MIT）只依赖这里描述的 HTTP 行为，
不依赖 KychNet 的内部表结构或源码；主服务实现见 KychNet 仓库的 `src/workflow/runner_api.rs`。

## 1. 认证

除 `register` 外，所有请求带：

```
Authorization: Bearer <runner token>
```

服务端存 `workflow_runners.token_hash`（SHA-256），明文只在注册时回显一次。
`enabled=false` 或撤销 token 后，所有请求（含已在执行的 run）立即 403 —— 这就是「临时禁用」。

所有回报请求都带 `run_id`（路径）+ `attempt`（body）。服务端做幂等校验：租约超时后
旧 runner 的回报不能覆盖新 attempt 的结果。

## 2. 端点

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `POST` | `/api/runner/register` | 用一次性注册码换 `{runner_id, token}` |
| `GET` | `/api/runner/jobs/claim` | 长轮询领 job；无 job 返回 **204** |
| `GET` | `/api/runner/jobs/{id}/snapshot?kind=both` | 模式 B 快照，tar.gz 流式 |
| `POST` | `/api/runner/jobs/{id}/start` | 回报工作流路径，预建 pending checks |
| `POST` | `/api/runner/jobs/{id}/logs` | 追加日志分片 |
| `POST` | `/api/runner/jobs/{id}/steps` | 批量回报 job/step 状态 |
| `POST` | `/api/runner/jobs/{id}/complete` | 结论 + 退出码 |
| `POST` | `/api/runner/heartbeat` | 续租 + 在线状态 |

### register

```json
// 请求
{ "name": "runner-1", "code": "<一次性注册码>", "labels": ["self-hosted","linux"],
  "protocol_version": 1, "snapshot_source": "api" }
// 200
{ "runner_id": "…", "token": "…" }
```

### claim

`GET /api/runner/jobs/claim?protocol_version=1&wait=30&labels=self-hosted,linux`

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

- `commit_oid` = **代码快照**（事件对应 commit；PR 事件为来源分支头）。
- `def_commit_oid` = **定义快照**，恒为默认分支 head。工作流 YAML 只从这里读，
  PR 分支里改工作流不生效。
- `trust_level` 由主服务强制：`pr` / `branch` 时 `secrets` 必须为空且不得发布；
  `default` / `manual` 才可能有 secrets。执行器会二次校验，不信任 YAML 自声明。
- **没有 `repo_url`、没有 `git_token`、没有 `pg_dsn`** —— 执行器不需要 smart HTTP，也不需要数据库。

### snapshot ★ 模式 B 核心

`GET /api/runner/jobs/{run_id}/snapshot?kind=code|definitions|both`

```
→ 200  Content-Type: application/gzip   （tar.gz 流式，边读边打包）
  X-Snapshot-Commit:      <code commit oid>
  X-Snapshot-Def-Commit:  <默认分支 head oid>
  X-Snapshot-Files:       <文件数>
  X-Snapshot-Bytes:       <原始字节数>
```

**tar 布局**（执行器按前缀分流，见 `src/snapshot.rs`）：

```
code/<仓库内相对路径>      完整代码快照，来自 commit_oid
definitions/<文件名>       默认分支 workflow_dir 下的 *.yml|*.yaml，来自 def_commit_oid
```

- `definitions/` 只放文件名（不含目录层级），执行器会覆盖到 `workspace/<workflow_dir>/`。
- 主服务打包时把 LFS pointer 还原为真实字节；缺失应返回明确错误 `lfs_unavailable`。
- 超过 `max_snapshot_bytes`（默认 200MB）服务端中断，执行器也会在流式下载时二次限额。
- 建议按 `(project_id, commit_oid)` 内容寻址缓存 tar.gz —— Git 对象不可变。

**服务端必须校验（任一不满足返回 403）**：

1. runner token 有效且 `enabled=true`；
2. `workflow_runs.executor_id == 当前 runner.id`；
3. `workflow_runs.status='in_progress'` 且 `lease_until > now()`；
4. 请求的 commit 属于该 run（`code` 用 `commit_oid`，`definitions` 用 `def_commit_oid`）。

这样 runner 只能取自己正在执行的那个 run 的快照，不能越权读别的项目。

### start / logs / steps / complete

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
{ "protocol_version": 1, "snapshot_source": "api",
  "act_version": "act version 0.2.xx", "labels": ["self-hosted"] }
```

服务端更新 `last_seen_at`，并把该 executor 名下所有 `in_progress` run 的
`lease_until` 续到 `now() + 5min`。心跳间隔 60s。

## 3. 状态机与降级

| 状态 | 触发 | 行为 |
| --- | --- | --- |
| 未安装 | 无存活 runner | 新 run 直接 `skipped / no_runner`；不阻塞 push；内置 `site.build` 照常 |
| 临时禁用 | `enabled=false` / 撤销 token | 同上；已 `queued` run 保留到 `expires_at`，恢复后可重放 |
| 安装并启用 | 存活且协议兼容 | `queued → claimed → in_progress`（带租约） |
| 心跳超时 | `lease_until < now()` | 打回 `queued`，`attempt+1`；超过 3 次标 `failure / runner_lost` |
| 队列积压 | 超过 `WORKFLOW_QUEUE_MAX` 或事件超过 TTL(24h) | 最老事件标 `expired` 并告警 |
| 快照不可用 | 对象被 gc / 主服务忙 | `failure / snapshot_unavailable`，**不影响内置动作** |

UI 必须区分 `skipped / no_runner`、`skipped / disabled`、`expired`、`failure` —— 否则
「没装执行器」会被误读成工作流失败。

**执行器缺席时内置动作照跑**：`site.build` / `build_page` 由主服务 `BuildQueue` 独立排队，
与 KychRunner 是否存在、YAML 工作流是否成功都无关。可选 `publish_on_check_success`
（默认 false）才让发布等待必需 checks 全绿。

## 4. 明确不做

- 不提供 `pg_dsn` / 只读视图 / 直连 PostgreSQL 通道（只做模式 B）。
- 不 clone、不 `git init`、不生成 `.git`、不在工作流里调用 git。
- 不在主服务进程内执行任何工作流步骤，主服务不依赖 Docker 或 act。
- 不接入 `actions/runner` / GitLab Runner / Gitea `act_runner`（都是平台 agent，不是独立执行器）。
