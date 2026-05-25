# livesh

A persistent live shell. `livesh` runs your default shell inside a daemon-owned
PTY so you can detach from it, reconnect later, and let other tools (terminal
multiplexers, editors, IDE panes) attach to the same session by id.

When the shell exits, the session is cleaned up automatically. The daemon also
sweeps dead sessions on startup, and bounds scrollback / snapshot / event-log
size so long-running shells stay cheap.

## Why livesh: surviving cmux restarts

livesh was built primarily as a **shell backend for [cmux](https://github.com/manaflow-ai/cmux)**.
cmux panes today are bound to the cmux process tree: when cmux quits or
restarts, the running shell dies and any in-flight `vim` buffer, REPL state, or
long-running interactive process is lost. `vault.agents` solves this for LLM
CLIs that have their own session-id concept, but ordinary `zsh` / `bash` panes
have no equivalent.

livesh fills that gap. The daemon owns the PTY out-of-process, so:

- **Close a pane** → bridge exits, shell stays alive in the daemon (detached).
- **Quit cmux** → every shell stays alive.
- **Relaunch cmux** → `livesh --open sh_<uuid>` reattaches the original PTY,
  vim buffer / REPL / running command intact.
- **Explicit kill** → `liveshctl kill <id>` terminates the shell and cleans
  state; "close pane" and "kill terminal" become distinct actions.

The wire contract (`--state-json-fd` for deterministic id capture, exit code
66 for "shell lost", 69 for "daemon unavailable", `liveshctl list --json` for
orphan discovery) is shaped specifically so cmux — or any pane-managing host —
can persist `liveShellId` in its layout and replay it on restore.

## Workspace

| Crate | Purpose |
|-------|---------|
| `livesh-protocol` | Wire types shared between client and daemon |
| `livesh-core`     | Session metadata, paths, GC, terminal model, limits |
| `livesh-cli`      | The `livesh`, `liveshd`, and `liveshctl` binaries |

## Build

```bash
cargo build --release
```

Or install into `$HOME/.local/bin`:

```bash
make install
# override prefix:
make install PREFIX=/usr/local
```

## Usage

```bash
# Create a new live shell and attach the current terminal to it
livesh

# Reattach to an existing session
livesh --open sh_<uuid>

# Bypass live mode and exec the real default shell directly
livesh --real

# Optional: name the session, or write its state JSON to a side-channel fd
livesh --name dev
livesh --state-json-fd 3 3>session.json
```

Manage sessions:

```bash
liveshctl list [--json]
liveshctl rename <sh_id> <name>
liveshctl kill <sh_id>
liveshctl gc
liveshctl status
```

The daemon (`liveshd`) is spawned on demand by the client; you don't normally
run it by hand.

## Requirements

- Rust 1.85+ (edition 2024)
- Unix-like OS (uses PTYs via `nix` / `portable-pty`)

## License

MIT — see [LICENSE](LICENSE).
