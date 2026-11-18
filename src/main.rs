mod client;
mod daemon;
mod mcp;
mod noise;
mod session;
mod text;

use anyhow::{bail, Context, Result};
use serde_json::json;

const HELP: &str = "shlim — a token-lean persistent shell for AI agents

usage:
  shlim mcp                  MCP server over stdio (for Claude Code, Codex, ...)
  shlim install <claude|codex> [--no-note]   register shlim with an agent (and tell it to prefer shlim)
  shlim doctor               check that the daemon runs and agents are set up to use shlim
  shlim run <command...>     run a command as the agent would and print its compact output
  shlim attach <id>          watch a session and type into it (Ctrl+\\ detaches)
  shlim list                 list sessions
  shlim read <id> [screen|all]
  shlim logs <id> [-f]       print a session's output; -f follows it until it exits
  shlim kill <id>
  shlim stats                tokens produced vs. sent to agents
  shlim stop                 stop the daemon and every session";

fn id(arg: Option<&String>) -> Result<u64> { arg.context("missing session id")?.trim_start_matches('#').parse().context("session id must be a number") }

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let print = |r: Result<String>| -> Result<()> { println!("{}", r?); Ok(()) };
    match args.first().map(String::as_str) {
        Some("mcp") => mcp::serve(),
        Some("daemon") => daemon::run(),
        Some("run") => print(client::request(json!({"op": "run", "cmd": args[1..].join(" "), "cwd": cwd}))),
        Some("read") => print(client::request(json!({"op": "read", "id": id(args.get(1))?, "mode": args.get(2)}))),
        Some("kill") => print(client::request(json!({"op": "kill", "id": id(args.get(1))?}))),
        Some("list") => print(client::request(json!({"op": "list"}))),
        Some("stats") => print(client::request(json!({"op": "stats"}))),
        Some("attach") => client::attach(id(args.get(1))?),
        Some("stop") => {
            if std::os::unix::net::UnixStream::connect(daemon::sock()).is_err() { println!("daemon not running"); return Ok(()) }
            print(client::request(json!({"op": "shutdown"})))
        }
        Some("logs") => logs(id(args.get(1))?, args.iter().skip(2).any(|a| a == "-f" || a == "--follow")),
        Some("doctor") => doctor(),
        Some("install") => install(args.get(1).map(String::as_str), !args.iter().any(|a| a == "--no-note")),
        Some("-V" | "--version") => { println!("shlim {}", env!("CARGO_PKG_VERSION")); Ok(()) }
        _ => { println!("{HELP}"); Ok(()) }
    }
}

const NOTE_BEGIN: &str = "<!-- shlim -->";
const AGENT_NOTE: &str = "<!-- shlim -->
## Shell commands
Run shell commands with the shlim MCP tools (`run`, `send`, `read`) instead of the built-in shell: output is compacted and sessions persist (dev servers, REPLs, ssh).
Start long-running programs with `run`, then use `read` (with `wait`, `grep` or `mode=all`) rather than re-running a command to see more output.
<!-- /shlim -->
";

/// Appends the instruction to an agent's memory file, once.
fn add_note(path: &std::path::Path) -> Result<bool> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current.contains(NOTE_BEGIN) { return Ok(false) }
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)? }
    let sep = if current.is_empty() { "" } else if current.ends_with("\n\n") { "" } else if current.ends_with('\n') { "\n" } else { "\n\n" };
    std::fs::write(path, format!("{current}{sep}{AGENT_NOTE}"))?;
    Ok(true)
}

fn home_dir() -> String { std::env::var("HOME").unwrap_or_default() }
fn claude_dir() -> std::path::PathBuf { std::env::var_os("CLAUDE_CONFIG_DIR").map(Into::into).unwrap_or_else(|| format!("{}/.claude", home_dir()).into()) }
fn codex_dir() -> std::path::PathBuf { std::env::var_os("CODEX_HOME").map(Into::into).unwrap_or_else(|| format!("{}/.codex", home_dir()).into()) }

fn install(agent: Option<&str>, note: bool) -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    match agent {
        Some("claude") => {
            let ok = std::process::Command::new("claude").args(["mcp", "add", "--scope", "user", "shlim", "--", &exe, "mcp"]).status().context("is the `claude` CLI on PATH?")?;
            if !ok.success() { bail!("claude mcp add failed") }
            if note {
                let path = claude_dir().join("CLAUDE.md");
                println!("{} {}", if add_note(&path)? { "added instruction to" } else { "instruction already in" }, path.display());
            }
            println!("registered. Restart Claude Code, then run `shlim doctor`.");
        }
        Some("codex") => {
            let dir = codex_dir();
            let path = dir.join("config.toml");
            let current = std::fs::read_to_string(&path).unwrap_or_default();
            if current.contains("[mcp_servers.shlim]") { println!("already in {}", path.display()) } else {
                std::fs::create_dir_all(&dir)?;
                let block = format!("{}[mcp_servers.shlim]\ncommand = {exe:?}\nargs = [\"mcp\"]\n", if current.is_empty() || current.ends_with("\n\n") { "" } else { "\n" });
                std::fs::write(&path, current + &block)?;
                println!("added to {}", path.display());
            }
            if note {
                let path = dir.join("AGENTS.md");
                println!("{} {}", if add_note(&path)? { "added instruction to" } else { "instruction already in" }, path.display());
            }
        }
        _ => bail!("usage: shlim install <claude|codex> [--no-note]"),
    }
    Ok(())
}

/// Plain output of a session; with `follow`, keeps printing until the process exits.
fn logs(id: u64, follow: bool) -> Result<()> {
    let mut from = 0usize;
    loop {
        let reply = client::request(json!({"op": "tail", "id": id, "from": from}))?;
        let (head, body) = reply.split_once('\n').unwrap_or((&reply, ""));
        let mut it = head.split(' ');
        from = it.next().and_then(|n| n.parse().ok()).unwrap_or(from);
        let done = it.next() == Some("1");
        if !body.is_empty() { println!("{body}") }
        if !follow || done { return Ok(()) }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

fn doctor() -> Result<()> {
    let mark = |ok: bool| if ok { "ok  " } else { "FAIL" };
    match client::request(json!({"op": "ping"})) {
        Ok(v) => println!("{} daemon running (v{v}, socket {})", mark(true), daemon::sock().display()),
        Err(e) => println!("{} daemon: {e:#}", mark(false)),
    }
    let registered = std::fs::read_to_string(format!("{}/.claude.json", home_dir())).is_ok_and(|t| t.contains("\"shlim\""));
    println!("{} registered with Claude Code (~/.claude.json){}", mark(registered), if registered { "" } else { "  -> shlim install claude" });
    let note = |p: std::path::PathBuf| std::fs::read_to_string(p).is_ok_and(|t| t.contains(NOTE_BEGIN));
    let c = claude_dir().join("CLAUDE.md");
    let has_note = note(c.clone());
    println!("{} CLAUDE.md tells the agent to prefer shlim ({}){}", mark(has_note), c.display(), if has_note { "" } else { "  -> shlim install claude" });
    let codex = std::fs::read_to_string(codex_dir().join("config.toml")).is_ok_and(|t| t.contains("[mcp_servers.shlim]"));
    println!("{} registered with Codex (optional)", mark(codex));
    println!("Tip: after a session, `shlim list` should show sessions created by your agent; if it is empty, the agent used its built-in shell.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_is_added_once_and_keeps_existing_content() {
        let dir = std::env::temp_dir().join(format!("shlim-note-{}", std::process::id()));
        let path = dir.join("CLAUDE.md");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "# mine\nkeep me").unwrap();
        assert!(add_note(&path).unwrap());
        assert!(!add_note(&path).unwrap(), "second run must not duplicate");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\nkeep me\n\n<!-- shlim -->"), "{text}");
        assert_eq!(text.matches(NOTE_BEGIN).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
