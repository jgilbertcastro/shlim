# shlim

A token-lean, persistent shell for AI coding agents. One binary, any agent that speaks MCP (Claude Code, Codex, OpenCode, Cursor…).

- **Persistent PTY sessions** owned by a background daemon: dev servers, REPLs, `ssh`, `vim` keep running across tool calls and even across agents.
- **Compact output**: ANSI stripped, progress bars collapsed to their final state, repeated lines folded (`warn … (×50)`), long output cut to head + tail with a pointer to the rest.
- **Smart waits**: a call returns when the process exits, goes quiet, or **asks for input** (`Password:`, `[y/N]`, `>>>`), so the agent doesn't poll.
- **Full-screen apps** (vim, htop) are returned as the rendered screen, not the escape-code stream.
- **You can watch and take over**: `shlim attach <id>` mirrors a session in your terminal.
- **Per-tool filters**: routine lines from `dotnet`, `npm/pnpm/yarn`, `docker`, `git` and `cargo` (restore chatter, per-test "Passed", layer progress, one line per crate) are omitted; errors and results stay. Everything is still available with `read mode=all`.
- **Measured savings**: `shlim stats` compares what a plain shell would have returned for the output the agent *actually read* with what it was sent. Output nobody read (a dev server's logs) is reported separately and not counted as savings. Tokens are estimated as bytes/4.

## Install

```bash
cargo install --path .
shlim install claude   # or: shlim install codex
shlim doctor           # checks the daemon, the registration and the agent instruction
```

`install` also appends a short instruction to `~/.claude/CLAUDE.md` (or `~/.codex/AGENTS.md`) telling the agent to prefer shlim; pass `--no-note` to skip that. Without it, agents tend to keep using their built-in shell.

## Tools exposed over MCP

| tool | what it does |
|---|---|
| `run {cmd, cwd?, timeout?}` | start a command in a new session; returns when it exits, idles or prompts |
| `send {id, text?, keys?, submit?}` | type into a session (`keys`: enter, tab, esc, arrows, `ctrl+c`…) and return the reply |
| `read {id, wait?, mode?, grep?, from?, limit?}` | new output since last read; `wait`: `exit`, `quiet` or a regex; `mode`: `screen` or `all` |
| `kill {id}` / `list` | stop / list sessions |

The whole tool schema is ~400 tokens.

## CLI

```
shlim run <command...>   shlim attach <id>   shlim list   shlim read <id> [screen|all]
shlim logs <id> [-f]     shlim kill <id>     shlim stats         shlim doctor        shlim stop
```

Limits (env): `SHLIM_MAX_SESSIONS` (64 running), `SHLIM_IDLE_MINUTES` (30), `SHLIM_LOG_DAYS` (7, old logs are deleted at daemon start).

State lives in `~/.shlim` (override with `SHLIM_HOME`): the daemon socket, `daemon.log` and the full raw log of every session under `logs/`.

## Status

Early. macOS and Linux only (Unix socket); Windows (ConPTY + named pipe) is planned. Sessions inherit the environment of whichever process first started the daemon.
