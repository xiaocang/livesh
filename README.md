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
# Create a new live shell and attach the current terminal to it.
# If liveshd is unreachable, livesh transparently exec's your real shell
# instead of erroring out.
livesh

# Reattach to an existing session
livesh --open sh_<uuid>

# Upgrade a fallback (real-shell) terminal back into a managed session.
# Same as plain `livesh`, but errors loudly if the daemon is still down —
# use this when you explicitly want managed mode.
livesh upgrade

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
liveshctl gc                              # also reaps dead-pid shells
liveshctl status
liveshctl upgrade-daemon [<new-binary>]   # hot-swap liveshd in place
```

The daemon (`liveshd`) is spawned on demand by the client; you don't normally
run it by hand.

## Hot-upgrading the daemon

`liveshctl upgrade-daemon [<new-binary>]` swaps the running `liveshd` for a
new binary without losing live sessions. With no argument it re-execs the
current binary path (handy after `make install`). Bridge clients (`livesh
--open`) reconnect on next use.

## Out of fds

If too many live shells exhaust the process fd limit, `livesh` will prompt
to kill detached shells, starting with the oldest:

1. `Kill detached shells idle for >3d? [y/N]`
2. `Kill detached shells idle for >1d? [y/N]`
3. `Kill ALL detached sessions? [y/N]`

Answer `y` to free fds and continue, `n` to abort.

## Detecting livesh from inside the shell

The daemon sets `LIVESH_SHELL_ID` in the inner shell's environment to the
session id (e.g. `sh_5f0c…`). This is a stable contract — child processes can
test for the variable to detect that they're running under livesh, and the
value is a valid argument for `livesh --open` / `liveshctl` commands. The
variable is set after `LIVESH_STRIP_PREFIX_ENV` filtering, so it cannot be
inadvertently stripped.

```bash
if [[ -n "$LIVESH_SHELL_ID" ]]; then
  echo "inside livesh session $LIVESH_SHELL_ID"
fi
```

## Stripping env vars from the inner shell

Hosts like cmux inject identifying env vars (`CMUX_*`, etc.) into every pane.
Those vars are inherited by `livesh`, forwarded to `liveshd`, and would
normally end up in the inner shell — where downstream tools (vault scanners,
agent CLIs like `claude`) pick them up and bind the process to a session
they shouldn't.

Set `LIVESH_STRIP_PREFIX_ENV` (comma-separated prefixes) in the environment
that `liveshd` is spawned from, and the daemon will drop matching keys from
both the client-supplied env and its own inherited env before spawning the
shell:

```bash
export LIVESH_STRIP_PREFIX_ENV=CMUX_
# multiple prefixes:
export LIVESH_STRIP_PREFIX_ENV=CMUX_,GHOSTTY_
```

Because `liveshd` is auto-spawned by `livesh`, exporting the var in your shell
profile (or whatever sets up the cmux/Ghostty session) is enough — no flag
needed on either binary. Already-running daemons need to be killed (`pkill
liveshd`) so the next `livesh` invocation respawns one with the new env.

## Requirements

- Rust 1.85+ (edition 2024)
- Unix-like OS (uses PTYs via `nix` / `portable-pty`)

## License

MIT — see [LICENSE](LICENSE).
