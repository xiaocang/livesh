# cmux × livesh 集成方案

> 目标：让 cmux 用 livesh 作为 terminal 后端，实现 **pane 关闭后 shell 仍活、cmux 重启后能精确接回原来那个 shell**。

## 0. TL;DR

livesh 已经写完了 daemon + create + open + bridge。`command = livesh` 写进 Ghostty config 后，cmux 新开 pane 已经会走 livesh —— 但 **cmux 不知道 `sh_<uuid>` 是什么，重启后只会再 fork 一个全新 shell，老的 sh state 留在 daemon 里成为孤儿**。

补完集成的关键就一件事：**让 sh_id 从 livesh 流回 cmux，并写进 pane 的 restore command**。

围绕这一点有三条路径：

| 路径 | 改 livesh | 改 cmux | 收益 |
|---|---|---|---|
| A. 现状 | ✗ | ✗ | 几乎无 —— 每个 pane = 新 shell，重启全丢 |
| B. livesh 自我 reexec + cmux 配 `vault.agents` | ✓ 小改 | ✗ 只改 `cmux.json` | 重启能恢复，detached shell 可见，无需改源码 |
| C. cmux 一等公民支持 | ✗（已就绪） | ✓ 改源码 | UI 完整，能列举/接管 orphan、显式 detach、kill 区分 |

推荐：B 先落地验证，C 作为长期方向给 cmux 提 PR。

---

## 1. 现状盘点

```
~/.config/ghostty/config
  command = /Users/jiahao.wang/.local/bin/livesh
```

cmux pane → Ghostty embed → 执行 `livesh`（无参）→ livesh `connect_or_spawn` 启动 liveshd → `CreateShell` → 拿到 `sh_<uuid>` → bridge 当前 TTY。

**问题**：`sh_<uuid>` 只活在 livesh 进程的 stack 里。cmux 的 pane 持久化机制（`resumeCommands` 签名白名单 + workspace layout）保存的是 argv `["livesh"]`，里面没有 id。

后果：

- cmux 关闭 pane → bridge 退出 → daemon 里那个 shell 变成 orphan，没人接得回。
- cmux 重启 → 重新跑 `["livesh"]` → 再开一个新 shell。
- 累积下来 `liveshctl list` 会越来越长，全是 cmux 接不回的 detached 会话；只能靠 daemon 自己 crash 时的 startup GC 清理。

`command = livesh` 单独使用的收益只剩：**Ghostty pane 内进程崩溃时 PTY 不死** —— 而 cmux 本来就会把 pane process 看作子进程，进程崩了 pane 就关。所以**净收益约等于 0**。

---

## 2. 系统层级和生命周期

```
┌──────────────────────────────────────────────────────────────┐
│ cmux app                                                     │
│  └─ pane                                                     │
│      └─ Ghostty terminal engine (PTY layer #1, cmux 看的)    │
│          └─ livesh (bridge process)                          │
│              ↕ Unix socket /tmp/livesh-<uid>/liveshd.sock    │
│                                                              │
│ liveshd (独立常驻 daemon)                                    │
│  └─ ShellState sh_<uuid>                                     │
│      ├─ PTY master (layer #2, daemon 看的)                   │
│      ├─ real shell process (zsh/...)                         │
│      ├─ vt100 parser + scrollback ring                       │
│      └─ event ring (用于 reattach 补发)                       │
└──────────────────────────────────────────────────────────────┘
```

**双层 PTY**。Ghostty 那层负责把 cmux 的窗口尺寸、键盘、剪贴板桥到 livesh 的 stdin/stdout；livesh 那层把字节透传到 daemon 的 PTY master。SIGWINCH 经过两跳传给 inner shell。

### 1.1 生命周期对照表

| 事件 | bridge 进程 | liveshd shell | cmux 应做的 |
|---|---|---|---|
| 用户在 shell 里 `exit` | 退出（exit code = shell 的） | 清理 state，删 metadata | 关闭 pane，**不**保留 sh_id |
| cmux 关闭 pane | SIGHUP → bridge 退出 | **保持存活**（detached） | 把 sh_id 留在 layout 里，下次能接回 |
| cmux 整个 app 退出 | bridge 全死 | 全部存活 | 保留 sh_id 列表 |
| 机器重启 / liveshd 被 kill -9 | bridge 收到 EOF，exit 69 | 全部丢失 | 启动时跑 `livesh --open` 会**警告 + 自动新建一个 shell** 顶上，pane 仍可用 |
| 用户点 "Kill Terminal" | bridge 退出 | `liveshctl kill sh_<uuid>` | 调 `liveshctl kill`，从 layout 移除 |

关键边界：`exit` 和 "关闭 pane" 必须区分。前者销毁 shell、后者只断 bridge。

---

## 3. 集成路径 A：现状（`command = livesh`）

**改动**：仅 `~/.config/ghostty/config` 加一行。

**能力**：
- 新 pane 都跑 livesh。
- `liveshctl list` 能看到所有正在跑的 shell。
- 用户在另一个终端窗口里手动跑 `livesh --open sh_xxx` 可以接管 cmux pane 里的 shell —— 但这会触发 steal，cmux 那边的 pane 会被 `DetachedByAnotherClient` 踢掉，pane 看起来就死了。

**不能**：
- cmux 重启后接回原 pane 的 shell。
- cmux UI 上看到哪个 pane 对应哪个 sh_id。

**结论**：作为过渡 OK，不是终态。

---

## 4. 集成路径 B：livesh 自 reexec + cmux 配置层接入

核心想法：**让 livesh 在 CreateShell 成功后立即 `execvp` 成 `livesh --open <sh_id>`**，从而：

1. 长寿命的 bridge 进程的 argv **自带 sh_id**，`ps`、`/proc`、cmux 的 process inspector 都能看到。
2. cmux 的 `vault.agents` 机制可以用 `sessionIdSource: argvOption("--open")` 抓到 id。
3. cmux 重启时，`resumeCommands` 签名白名单里写的就是 `["livesh", "--open", "sh_<uuid>"]`，原样重放就是接回同一个 shell。

### 4.1 livesh 端改动

文件：`crates/livesh-cli/src/bin/livesh.rs`（或 `main.rs` 等价位置）

```rust
// 在 LiveshMode::New 分支里，CreateShell 成功之后、bridge 之前：
let created = client.create_shell(...).await?;
if let Some(fd) = state_json_fd {
    state_json::write(fd, &created)?;
}

// 新增：reexec 自身，argv 里带上 --open
if std::env::var_os("LIVESH_INTERNAL_NO_REEXEC").is_none() {
    let exe = std::env::current_exe()?;
    let id = created.id.to_string();
    let err = Command::new(&exe)
        .arg("--open").arg(&id)
        .env("LIVESH_INTERNAL_NO_REEXEC", "1")  // 防递归保险
        .exec();
    return Err(err.into());
}
```

注意：

- `LIVESH_INTERNAL_*` 已经在 `filtered_current_env()` 里被过滤掉，daemon 不会把它传给 inner shell。
- `state-json-fd` 必须在 reexec 之前写入并 close，因为 fd 不一定能跨 exec 保留 cloexec 设置。
- 如果 stdout/stderr 被重定向到非 TTY，reexec 仍然安全。
- 如果 `--state-json-fd` 是宿主主动传的，reexec 不影响——宿主已经读到 JSON。

成本：约 30 行 Rust + 单测一条（"reexec only happens once when no LIVESH_INTERNAL_NO_REEXEC"）。

### 4.2 cmux 端改动

只改 `~/.config/cmux/cmux.json`，注册 livesh 作为一个 "vault agent"：

```json
{
  "vault": {
    "agents": [
      {
        "id": "livesh",
        "name": "Live Shell",
        "detect": {
          "processName": "livesh",
          "argvContains": "--open"
        },
        "sessionIdSource": {
          "type": "argvOption",
          "argvOption": "--open"
        },
        "resumeCommand": "livesh --open {{sessionId}}",
        "cwd": "preserve"
      }
    ]
  }
}
```

第一次新建 pane 时 cmux 仍然跑 `livesh`（来自 Ghostty `command =`）。livesh 自我 reexec 之后 cmux 的进程检测会发现："这个 pane 里运行了 livesh `--open sh_xxx`"，于是把 `sh_xxx` 当作 vault session id 记下来。

之后：

- cmux 重启时调用 vault 的 `resumeCommand` → `livesh --open sh_xxx` → 接回同一个 shell。
- cmux 的命令面板里会出现 "Resume Live Shell sh_xxx"。
- `autoResumeAgentSessions: true`（默认）会自动恢复。

### 4.3 还差什么

- **看不到 detached/orphan 的 livesh**：vault 只跟踪它见过的 pane。`liveshctl list` 里别的 shell（cmux 之外创建的）不会出现在 vault 列表。这是 cmux 模型的限制，路径 B 解决不了。
- **首次进入 pane 时**，从 `livesh` 启动到 `--open sh_xxx` reexec 完成之间有几十毫秒空窗。cmux 如果在这个空窗里抓 argv 会拿到 `["livesh"]`。需要检查 cmux 的 detect 实现是否 retry / poll。如果它只在 pane spawn 时抓一次，那 reexec 之后还得给 cmux 一个 trigger（OSC 序列、文件 watcher……）。**这是路径 B 的最大风险点**，建议先写个最小验证脚本测一下。
- **`vault.agents.detect` 的判断粒度**：`processName: "livesh"` + `argvContains: "--open"` 是字符串匹配。如果用户也手动 `livesh --open sh_yyy`，会被 cmux 误认成 "运行在 pane 里的 agent"。一般无害。

---

## 5. 集成路径 C：cmux 一等公民支持

cmux 接管 pane spawn 流程，不再透过 Ghostty 的 `command =` 启动 livesh，而是显式调用：

```bash
livesh --name "<workspace>/<surface>" --state-json-fd 3 3>"$TMPDIR/cmux-pane-<uuid>.json"
```

读 JSON 拿到 `id`，写进 pane 的持久化 layout：

```json
{
  "kind": "terminal",
  "title": "shell",
  "liveShellId": "sh_2e7b9e9b...",
  "openCommand": ["livesh", "--open", "sh_2e7b9e9b..."]
}
```

重启时按 `liveShellId` 跑 `livesh --open`。

### 5.1 cmux 需要改的点

| 区域 | 改动 |
|---|---|
| Pane spawn | 接收 `--state-json-fd` 的 fd，解析 JSON，存 `liveShellId` |
| Pane 元数据 | schema 加 `liveShellId: string \| null` |
| Restore 流程 | 有 `liveShellId` → 用 `openCommand`；没有 → 走旧的 fallback |
| 关闭 pane 语义 | "Close pane" 仅 SIGHUP livesh bridge；新增 "Kill terminal" 调 `liveshctl kill <id>` |
| Exit code 处理 | livesh exit code 66 = shell lost → UI 提示 "this live shell no longer exists"，给 "Start new shell" 按钮，不要静默重启 |
| Exit code 69 | daemon unavailable → 提示用户 |
| 命令面板 | "Attach to running live shell..." → 调 `liveshctl list --json`，列 detached shell 给用户选 |
| Pane chrome | 显示 `sh_xxx` + attached/detached 指示器 |
| Settings UI | 选项："Use livesh as default shell"，写 schema 字段 `terminal.shellBackend: "livesh" \| "direct"` |

### 5.2 livesh 端可加的 quality-of-life（非必需）

- `--cwd <path>`：当前 `current_dir()` 隐式取宿主进程的 cwd，cmux 显式传更稳。
- `--env-from-fd <fd>`：避免 cmux 把整个 env 通过 argv / 进程 env 传（敏感变量也能控制）。
- `liveshctl list --json --filter detached`：cmux 用来填 "可接管" 列表。
- IPC 直连：cmux 如果不想 fork `liveshctl` 子进程，可以直接连 `/tmp/livesh-<uid>/liveshd.sock` 走 protocol —— 这意味着要 ship livesh-protocol 的 schema / TS binding。
- Daemon 事件流：当前协议是请求-响应。cmux 想知道 "某个 detached shell 突然 exited"，需要 server-push。可以加 `ClientMsg::Subscribe { events: [Exited, Created] }` + server 主动推 `Event { ... }`。

---

## 6. 跨路径都要处理的边界

### 6.1 环境变量

cmux 注入的 `CMUX_*`、Ghostty 注入的 `GHOSTTY_*`、`TERM_PROGRAM=ghostty` 这些都必须穿透到 inner shell。livesh 现在的实现：`filtered_current_env()` 只剔除 `LIVESH_INTERNAL_*`，其它原样转发 —— OK，但要在 cmux 集成测试里验一遍。

shell-integration 注入的 `GHOSTTY_SHELL_INTEGRATION_*`、`GHOSTTY_RESOURCES_DIR` 等 zsh/bash 启动钩子靠的是 env + 改 PATH，**不**依赖 argv[0]，所以套一层 livesh 仍然生效。

### 6.2 cwd

- 默认行为：livesh 取 `current_dir()`，即 bridge 进程的 cwd。
- cmux 给 pane 设的工作目录是通过 `chdir` 还是 `cmd.cwd()`？取决于 cmux 内部实现。如果它 `chdir` 后再 exec，OK；如果它把 cwd 作为参数传，需要 livesh 接 `--cwd`。

### 6.3 双层 scrollback

- Ghostty 自己有 `scrollback-limit = 50000000`，cmux 的搜索、复制、复制为 ANSI 都基于这层。
- livesh daemon 也维护一个 vt100 parser + scrollback ring（用于 detach 后 reattach 重画屏幕）。
- **重复存储**，浪费内存。但功能正交：Ghostty scrollback 关 pane 就丢；livesh scrollback 在 pane 关了之后还在。可以接受。
- 注意：livesh 的 `scrollback_bytes_per_shell = 10 MiB`（来自 plan.md §7）远小于 Ghostty 的 50 GB 上限。reattach 后用户能往回滚的只有 10 MiB —— 必要时调大 livesh 配置或让 Ghostty 在 reattach 时主动 OSC 1337 请求历史。

### 6.4 与 agent 的关系（claude-teams / codex-teams / opencode）

cmux 已经有 vault.agents 给 Claude/Codex/Pi 用。两种叠加方式：

**方式 A：agent 不走 livesh。** cmux 启动 `claude-teams` 直接 fork claude CLI，agent 自己有 session resume。
**方式 B：agent 也跑在 livesh 里。** pane 进程 = `livesh` → 内部 shell = zsh → zsh 里跑 `claude`。两层 session id（livesh 的 sh_id + claude 的 session_id）。restore 时 livesh 先接回 PTY，PTY 里 claude 进程已经在跑 → 不用走 vault 的 `resumeCommand`。

方式 B 更省心但调试更难。方式 A 更干净。推荐 **agent pane 不套 livesh**（cmux 可以配置 per-command 后端选择）。

### 6.5 单 attach 限制

livesh V1 是单 client + steal-on-open。cmux 的 split view 把同一个 pane 显示成两份是不可能的 —— 但 cmux 模型里本来 pane = surface，一对一，所以这不是问题。**如果未来 cmux 加 "mirror pane to another window"**，需要 livesh 支持 read-only 第二 attach（plan.md §15 提到 P2）。

### 6.6 macOS / SIP / 沙盒

- liveshd 通过 Unix domain socket 在 `/tmp/livesh-<uid>/` 通信，**默认不在 cmux 的 macOS App Sandbox container 路径下**。如果 cmux 是 sandboxed app，可能没法访问 `/tmp/livesh-<uid>/`。
- 解决：cmux 需要 `com.apple.security.temporary-exception.files.absolute-path.read-write` entitlement，**或者** livesh 接 `LIVESH_RUNTIME_DIR` 让 cmux 指定一个 sandbox-friendly 路径（建议 livesh 增加该 env 支持，目前只看 `XDG_RUNTIME_DIR`）。

### 6.7 多用户 / SSH

如果 cmux 启用了 "open SSH workspace"，pane 实际跑在远端。远端没装 livesh 就退回直接 shell —— `command = livesh` 会失败。需要：

- Ghostty `command = sh -c 'command -v livesh >/dev/null && exec livesh || exec $SHELL'`，或者
- livesh 在 `--real` fallback 之前再加一层 "if not found, just exec shell" —— 不行，livesh 不存在就根本调不到它自己。第一种是唯一可行方案。

---

## 7. 分阶段落地

### Phase 0（已完成）
- [x] `command = livesh` 写进 Ghostty config。

### Phase 1：livesh 自 reexec（已完成）
- [x] `crates/livesh-cli/src/bin/livesh.rs`：CreateShell 成功后 `execvp("livesh", ["--open", id])`，受 `LIVESH_INTERNAL_NO_REEXEC` 门控。
- [x] `crates/livesh-cli/src/args.rs`：测试覆盖 `--cwd` 解析（reexec 后的 `--open` 路径走 `LiveshMode::Open` 已有测试）。
- [ ] 手动验：`ps -axo pid,command | grep livesh` 在 pane 里能看到 `livesh --open sh_xxx`。
- [ ] `cargo build --release && make install` 部署新二进制。

### Phase 2：cmux vault.agents 配置（已完成）
- [x] `~/.config/cmux/cmux.json` 加 `vault.agents[livesh]` 条目。
- [ ] `cmux reload-config` 生效（需要 cmux 在跑）。
- [ ] 验证：关 pane → 重启 cmux → pane 自动恢复 → 命令历史 / vim buffer 仍在。

### Phase 3：sandbox / 边界打补丁（已完成）
- [x] livesh 支持 `LIVESH_RUNTIME_DIR` env override (`crates/livesh-core/src/paths.rs`)。
- [x] livesh 支持 `--cwd <path>` 显式参数 (`crates/livesh-cli/src/args.rs`)。
- [ ] 文档：在 README 加 "macOS sandbox 用户配置" 小节。

### Phase 4：cmux 上游 PR（路径 C）
- [ ] 给 manaflow-ai/cmux 提 issue：propose live-shell backend。issue 草稿见 §10。
- [ ] Schema 扩展：`terminal.shellBackend`、pane layout `liveShellId`。
- [ ] UI：detached shell list、kill 与 close 区分、exit code 66/69 提示。
- [ ] 关键设计点：与 vault.agents 模型的关系（livesh 应不应该也走 vault，还是独立 backend？倾向独立 backend，因为 livesh 不是 agent）。

---

## 8. 风险清单

| 风险 | 等级 | 缓解 |
|---|---|---|
| cmux vault detect 在 livesh reexec 完成之前抓 argv，导致 sessionId 抓不到 | **中-高** | 路径 B 落地前必须验证。如果撞上，需 livesh 在 reexec 后写 OSC 序列或 sentinel 文件让 cmux 重新扫描。 |
| 双层 PTY 在复杂 TUI（tmux、neovim、mosh）下行为异常 | 中 | plan.md §19 已说明：先 smoke test 主流 TUI；livesh vt100 parser 不完整时再升级。 |
| liveshd 崩溃 → 所有 cmux pane 同时变僵 | 中 | exit 69 UI 提示要清晰。cmux 应在 pane chrome 显示 "daemon unavailable"，不要悄悄关 pane。 |
| macOS App Sandbox 拦 socket | 中 | LIVESH_RUNTIME_DIR override + 文档说明。 |
| cmux 升级改 vault.agents schema，路径 B 失效 | 低 | schemaVersion 已是 1，破坏性变更概率小；路径 C 才是终态。 |
| 用户 chsh 把 livesh 设为系统 default shell 后又跑 cmux | 低 | shell_resolve.rs 已有 livesh 自环检测，会落到 /bin/zsh。 |
| livesh shell 输出量大 → daemon 内存涨 | 低 | scrollback / event ring 已有 cap（plan.md §18）。 |

---

## 9. 验收

集成完成的标准：

1. 新建 cmux pane，运行 `vim`，写几行，**不**保存。
2. 关闭 pane（cmd+w），重启 cmux。
3. 恢复后 vim 仍在编辑同一个 buffer，光标位置一致。
4. `liveshctl list` 中没有 orphan（pane 数量 == shell 数量）。
5. 在 shell 里 `exit`，pane 关闭，`liveshctl list` 中对应 sh_id 消失。
6. 杀掉 `liveshd`（`liveshctl status` 看 pid 后 `kill -9`），cmux pane 显示明确错误而非静默死掉。
7. `cmux reload-config` 后所有 pane 仍能正常输入。

---

## 10. cmux 上游 issue 草稿（Path C）

下面是给 `manaflow-ai/cmux` 提 issue 的内容，按需复制：

---

### Title

Propose: first-class "live shell backend" so terminals survive cmux restart

### Body

**Problem**

cmux's terminal panes today are bound to the lifetime of the local process tree. When cmux quits or restarts, the running shell dies and `resumeCommands` can only re-launch a fresh shell — any in-flight `vim` buffer, REPL state, or long-running interactive process is lost. The `vault.agents` mechanism solves this for explicit "agent CLIs" (Claude, Codex, Pi) that have their own session-id concept, but ordinary `zsh` / `bash` panes have no equivalent.

**Proposal**

Add an optional **shell backend** that wraps each terminal pane in [livesh](https://github.com/<owner>/livesh), a small daemon that owns the PTY and outlives the cmux process. cmux would invoke `livesh --state-json-fd <fd>` on pane creation, receive a `sh_<uuid>` session id via the side channel, persist that id in pane layout, and run `livesh --open sh_<uuid>` on restore.

**Wire contract (already shipped by livesh)**

```bash
# create
livesh --name "$pane_title" --cwd "$workspace_root" --state-json-fd 3 3>"$tmp/state.json"
# JSON written to fd 3:
# { "schema": 1, "id": "sh_…", "name": "…", "status": "running",
#   "restore": ["livesh", "--open", "sh_…"] }

# restore
livesh --open sh_<uuid>
# exit code 66 = shell lost (daemon crashed or shell exited externally)
# exit code 69 = daemon unavailable

# kill (explicit user action only)
liveshctl kill sh_<uuid>
```

Lifecycle semantics:

| cmux event | What cmux does | Effect on shell |
|---|---|---|
| User closes pane (cmd+w) | SIGHUP livesh bridge | Shell stays alive in daemon, detached |
| cmux app quits | bridges exit | All shells stay alive |
| cmux relaunches | spawn `livesh --open <id>` per pane | Reattach with snapshot |
| User picks "Kill terminal" | run `liveshctl kill <id>` | Shell terminated, state cleaned |
| Shell exits internally (`exit`) | bridge returns with shell's exit code | State auto-cleaned by livesh daemon |

**What needs to change in cmux**

1. **Pane schema** — add nullable `liveShellId: string` to pane layout records.
2. **Pane spawn path** — when backend = livesh:
   - Allocate a pipe, pass write end as fd 3 via `posix_spawn` file actions.
   - After launching `livesh --state-json-fd 3 --cwd $cwd --name $title`, read JSON from the pipe, parse, store `liveShellId`.
   - Treat livesh exit code 66 as "shell no longer exists" (show banner: "This live shell is gone. [Start new]") rather than auto-restart.
   - Treat exit code 69 as "daemon unavailable" (show banner: "livesh daemon not running"); don't tear down the pane.
3. **Restore path** — if `liveShellId` present, exec `livesh --open <id>`; else fall back to current behavior.
4. **"Close pane" vs "Kill terminal"** — current behavior implicitly does both. Split into two actions:
   - Close pane: SIGHUP / SIGTERM the bridge process (shell survives).
   - Kill terminal: run `liveshctl kill <id>` before closing the pane.
5. **Setting** — add `terminal.shellBackend: "livesh" | "direct"` to schema, default `"direct"`; only activate the new spawn path when `"livesh"` and `livesh` is on PATH.
6. **Command palette** — "Attach to running live shell…" entry that calls `liveshctl list --json` and lets the user attach a new surface to a detached `sh_<uuid>` (useful for orphans, e.g. shells created outside cmux).
7. **Pane chrome (optional)** — show `sh_xxxx` and an attached/detached indicator dot.

**Why a new backend rather than reusing `vault.agents`**

`vault.agents` is designed around "this pane is running an LLM CLI". livesh isn't an agent — it's transport. Using vault for it works as a stopgap (we ship a config that does exactly this) but conflates two unrelated abstractions:

- vault assumes one agent process per pane, identified by process name; livesh wraps *every* pane.
- vault detection works post-hoc by reading argv; the proposed backend captures the id deterministically via fd 3, removing the race between pane spawn and process detection.
- vault `autoResumeAgentSessions` toggles all agents at once; users may want livesh always on but agent resume off.

**Non-goals**

- Cross-machine survival (livesh daemon lives on one host).
- Surviving daemon crash or machine reboot (livesh declines to fake this; exit 66 surfaces it honestly).
- Multi-pane mirror of the same shell (livesh V1 is single-attach).

**Compatibility**

Strictly opt-in via setting. Existing panes/users see no behavior change. Schema additions are nullable.

**Prior art / references**

- livesh source: `https://github.com/<owner>/livesh`
- design doc & wire contract: `livesh/plan.md` §23 ("cmux 集成契约")
- this proposal originated as `livesh/cmux-livesh.md`

Happy to send a PR scoped to (1)–(3) first if there's interest; (4)–(7) can land incrementally.

---

## 11. 落地 checklist（开发者视角）

```text
# 1. 重新构建 livesh 并部署
cd /Users/jiahao.wang/work/livesh
cargo build --release
make install            # 装到 ~/.local/bin/livesh

# 2. 验证 reexec 行为
livesh &
sleep 1
ps -axo pid,command | grep -E '\blivesh\b'
# 期望看到形如:  12345 livesh --open sh_<uuid>
# 不应该看到孤立的 `livesh`（无参）长存
kill %1

# 3. 验证 cmux.json
cmux config doctor
# 期望 keys 里包含 vault

# 4. 启动 cmux，开新 pane，跑 vim、写一点东西、不保存
# 5. cmd+w 关 pane，再 cmd+q 退出 cmux，再启动 cmux
# 6. 验证 pane 是否恢复（取决于 cmux vault 的 autoResume 行为是否覆盖非 LLM-agent 类型）

# 7. 兜底：手动通过命令面板 "Resume Live Shell" 检查能否拿回 buffer
```
