//! Background process that owns every session, so sessions outlive the agent that started them.
//! Protocol: one JSON request per line, one JSON response per line, over a Unix socket.
use crate::session::{Session, Until, COLS, ROWS};
use crate::text::Budget;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::{UnixListener, UnixStream}},
    path::PathBuf,
    sync::{atomic::{AtomicU32, Ordering}, mpsc, Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn home() -> PathBuf {
    std::env::var_os("SHLIM_HOME").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(".shlim"))
}
pub fn sock() -> PathBuf { home().join("shlim.sock") }

pub fn ensure_home() -> Result<()> {
    let dir = home();
    fs::create_dir_all(dir.join("logs"))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[derive(Default)]
struct Hub {
    sessions: Mutex<BTreeMap<u32, Arc<Session>>>,
    next: AtomicU32,
    /// Totals of sessions already removed.
    closed: Mutex<Totals>,
}

/// Plain bytes produced, plain bytes the agent asked to read, bytes actually sent to it.
#[derive(Default, Clone, Copy)]
struct Totals { produced: u64, read: u64, sent: u64 }

impl Hub {
    // A panic in one connection thread must not poison the whole daemon.
    fn sessions(&self) -> MutexGuard<'_, BTreeMap<u32, Arc<Session>>> { self.sessions.lock().unwrap_or_else(|e| e.into_inner()) }
    fn closed(&self) -> MutexGuard<'_, Totals> { self.closed.lock().unwrap_or_else(|e| e.into_inner()) }

    fn get(&self, r: &Value) -> Result<Arc<Session>> {
        let id = r["id"].as_u64().ok_or_else(|| anyhow!("missing id"))? as u32;
        self.sessions().get(&id).cloned().ok_or_else(|| anyhow!("no session #{id} (see list)"))
    }
    fn remove(&self, s: &Session) {
        self.sessions().remove(&s.id);
        let st = s.lock();
        let mut c = self.closed();
        c.produced += st.text.plain_bytes;
        c.read += st.read_bytes;
        c.sent += st.sent_bytes;
    }
}

pub fn run() -> Result<()> {
    ensure_home()?;
    let path = sock();
    if UnixStream::connect(&path).is_ok() { return Ok(()) }
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    // The home directory is already 0700; this makes the socket itself owner-only as well.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    prune_logs();
    let hub = Arc::new(Hub { next: AtomicU32::new(1), ..Default::default() });
    let reaper = hub.clone();
    thread::spawn(move || reap(&reaper));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let hub = hub.clone();
        thread::spawn(move || { let _ = serve(&hub, conn); });
    }
    Ok(())
}

/// Deletes session logs older than `SHLIM_LOG_DAYS` (default 7) so `~/.shlim/logs` cannot grow forever.
fn prune_logs() {
    let days: u64 = std::env::var("SHLIM_LOG_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(7);
    let Ok(dir) = fs::read_dir(home().join("logs")) else { return };
    for e in dir.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(days * 86_400));
        if old { let _ = fs::remove_file(e.path()); }
    }
}

/// Most sessions that may run at once (`SHLIM_MAX_SESSIONS`, default 64).
fn max_sessions() -> usize { std::env::var("SHLIM_MAX_SESSIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(64) }

/// Stops sessions nobody has used for `SHLIM_IDLE_MINUTES` (default 30) and that no human is watching.
fn reap(hub: &Hub) {
    let limit = Duration::from_secs(60 * std::env::var("SHLIM_IDLE_MINUTES").ok().and_then(|v| v.parse().ok()).unwrap_or(30u64));
    loop {
        thread::sleep(Duration::from_secs(60));
        let idle: Vec<Arc<Session>> = hub.sessions().values().filter(|s| {
            let st = s.lock();
            st.watchers.is_empty() && st.touched.elapsed() > limit && st.last_out.elapsed() > limit
        }).cloned().collect();
        for s in idle { s.kill(); hub.remove(&s); }
    }
}

fn serve(hub: &Hub, conn: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut w = conn;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 { return Ok(()) }
        let req: Value = serde_json::from_str(&line).unwrap_or_default();
        let op = req["op"].as_str().unwrap_or("");
        if op == "attach" { return attach(hub, &req, reader, w) }
        let resp = match handle(hub, &req) {
            Ok(text) => json!({"ok": true, "text": text}),
            Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
        };
        writeln!(w, "{resp}")?;
        if op == "shutdown" {
            for s in hub.sessions().values() { s.kill() }
            let _ = fs::remove_file(sock());
            std::process::exit(0);
        }
    }
}

fn key(k: &str) -> Result<Vec<u8>> {
    let k = k.to_lowercase();
    Ok(match k.as_str() {
        "enter" | "return" => b"\r".to_vec(),
        "tab" => b"\t".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "space" => b" ".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        _ => match k.strip_prefix("ctrl+").map(str::as_bytes) {
            Some([c @ b'a'..=b'z']) => vec![c - b'a' + 1],
            _ => bail!("unknown key {k:?}"),
        },
    })
}

fn age(secs: u64) -> String {
    match secs { 0..=119 => format!("{secs}s"), 120..=7199 => format!("{}m", secs / 60), _ => format!("{}h", secs / 3600) }
}

/// The command without a leading `cd <dir> &&`, which every agent adds and which hides the real command.
fn short_cmd(cmd: &str, max: usize) -> String {
    let body = match cmd.split_once(" && ") { Some((a, rest)) if a.trim_start().starts_with("cd ") => rest, _ => cmd };
    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max { one_line } else { format!("{}…", one_line.chars().take(max).collect::<String>()) }
}

fn kb(b: u64) -> String { if b < 1024 { format!("{b}B") } else { format!("{:.1}k", b as f64 / 1024.0) } }

fn handle(hub: &Hub, r: &Value) -> Result<String> {
    let secs = |d: f64| Duration::from_secs_f64(r["timeout"].as_f64().unwrap_or(d).clamp(0.0, 600.0));
    // Default reply budget ~1k tokens; an explicit `limit` means the agent wants more.
    let limit = match r["limit"].as_u64() {
        Some(n) => Budget { lines: n.clamp(1, 2000) as usize, bytes: 64 * 1024 },
        None => Budget { lines: 100, bytes: 4000 },
    };
    let quiet = Duration::from_millis(r["quiet_ms"].as_u64().unwrap_or(3000).clamp(100, 60_000));
    match r["op"].as_str().unwrap_or("") {
        "ping" => Ok(env!("CARGO_PKG_VERSION").into()),
        "shutdown" => Ok("bye".into()),
        "run" => {
            let cmd = r["cmd"].as_str().unwrap_or("");
            let cwd = r["cwd"].as_str().map(String::from).unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
            let running = hub.sessions().values().filter(|s| s.exited().is_none()).count();
            if running >= max_sessions() {
                bail!("{running} sessions are already running (limit {}); kill some with kill/list or raise SHLIM_MAX_SESSIONS", max_sessions())
            }
            let id = hub.next.fetch_add(1, Ordering::SeqCst);
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let log = File::create(home().join("logs").join(format!("{id}-{stamp}.log"))).ok();
            let s = Session::spawn(id, cmd, &cwd, log)?;
            hub.sessions().insert(id, s.clone());
            if r["wait"].as_bool() == Some(false) { return Ok(format!("[#{id} started]")) }
            Ok(s.wait_report(Until::Auto, quiet, secs(30.0), limit))
        }
        "send" => {
            let s = hub.get(r)?;
            if s.exited().is_some() { bail!("session #{} has exited", s.id) }
            let mut bytes = r["text"].as_str().unwrap_or("").as_bytes().to_vec();
            for k in r["keys"].as_array().into_iter().flatten() { bytes.extend(key(k.as_str().unwrap_or(""))?) }
            if r["submit"].as_bool() == Some(true) { bytes.push(b'\r') }
            s.send(r["text"].as_str().unwrap_or(""), &bytes)?;
            if r["wait"].as_bool() == Some(false) { return Ok(format!("[#{} sent]", s.id)) }
            Ok(s.wait_report(Until::Auto, quiet, secs(10.0), limit))
        }
        "read" => {
            let s = hub.get(r)?;
            let grep = r["grep"].as_str().map(regex::Regex::new).transpose().context("grep")?;
            let from = r["from"].as_u64().unwrap_or(0) as usize;
            match r["mode"].as_str().unwrap_or(if grep.is_some() { "all" } else { "new" }) {
                "screen" => Ok(s.screen()),
                "all" => Ok(s.lines(from, grep.as_ref(), r["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize)),
                _ => {
                    let until = match r["wait"].as_str() {
                        None => return Ok(s.wait_report(Until::Auto, quiet, Duration::ZERO, limit)),
                        Some("exit") => Until::Exit,
                        Some("quiet") => Until::Quiet,
                        Some("auto" | "input") => Until::Auto,
                        Some(p) => Until::Pattern(regex::Regex::new(p).context("wait pattern")?),
                    };
                    Ok(s.wait_report(until, quiet, secs(60.0), limit))
                }
            }
        }
        "kill" => {
            let s = hub.get(r)?;
            s.kill();
            let text = s.wait_report(Until::Exit, quiet, Duration::from_secs(2), limit);
            hub.remove(&s);
            Ok(text)
        }
        "list" => {
            let sessions = hub.sessions();
            if sessions.is_empty() { return Ok("no sessions".into()) }
            Ok(sessions.values().map(|s| {
                let st = s.lock();
                let state = st.exit.map_or("running".to_string(), |e| format!("exit {}", e.0));
                let unread = st.text.plain_bytes.saturating_sub(st.read_bytes);
                let unread = if unread >= 1024 { format!(" (+{} unread)", kb(unread)) } else { String::new() };
                format!("#{} {state} {} `{}` {}→{}{unread}", s.id, age(s.started.elapsed().as_secs()), short_cmd(&s.cmd, 70), kb(st.read_bytes), kb(st.sent_bytes))
            }).collect::<Vec<_>>().join("\n"))
        }
        "tail" => {
            let s = hub.get(r)?;
            let (next, done, lines) = s.tail(r["from"].as_u64().unwrap_or(0) as usize);
            Ok(format!("{next} {}\n{}", u8::from(done), lines.join("\n")))
        }
        "stats" => {
            let mut t = *hub.closed();
            let mut running = 0u64;
            for s in hub.sessions().values() {
                let st = s.lock();
                t.produced += st.text.plain_bytes;
                t.read += st.read_bytes;
                t.sent += st.sent_bytes;
                if st.exit.is_none() { running += st.text.plain_bytes.saturating_sub(st.read_bytes) }
            }
            // Only output the agent asked for counts: a log nobody read would have cost nothing either way.
            let saved = t.read.saturating_sub(t.sent);
            let pct = if t.read > 0 { saved as f64 * 100.0 / t.read as f64 } else { 0.0 };
            let unread = t.produced.saturating_sub(t.read);
            Ok(format!(
                "read by agent:   {} (~{} tokens, what a plain shell would have sent)\nsent to agent:   {} (~{} tokens)\nsaved:           ~{} tokens ({pct:.0}%)\nnever read:      {} (not counted; {} of it from running sessions)\ntokens are estimated as bytes/4",
                kb(t.read), t.read / 4, kb(t.sent), t.sent / 4, saved / 4, kb(unread), kb(running)
            ))
        }
        op => bail!("unknown op {op:?}"),
    }
}

/// Mirrors a session to a human terminal: raw output out, keystrokes in.
fn attach(hub: &Hub, req: &Value, mut reader: BufReader<UnixStream>, mut w: UnixStream) -> Result<()> {
    let s = match hub.get(req) {
        Ok(s) => s,
        Err(e) => { writeln!(w, "{}", json!({"ok": false, "error": e.to_string()}))?; return Ok(()) }
    };
    let rows = req["rows"].as_u64().unwrap_or(ROWS as u64) as u16;
    let cols = req["cols"].as_u64().unwrap_or(COLS as u64) as u16;
    s.resize(rows, cols);
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let screen = {
        let mut st = s.lock();
        st.watchers.push(tx);
        st.screen.screen().contents_formatted()
    };
    writeln!(w, "{}", json!({"ok": true, "screen": screen}))?;
    let mut out = w.try_clone()?;
    let watched = s.clone();
    thread::spawn(move || {
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(bytes) => if out.write_all(&bytes).is_err() { break },
                Err(mpsc::RecvTimeoutError::Timeout) => if watched.exited().is_some() { break },
                Err(_) => break,
            }
        }
        let _ = out.shutdown(std::net::Shutdown::Both);
    });
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = reader.read(&mut buf) { if s.write(&buf[..n]).is_err() { break } }
    s.resize(ROWS, COLS);
    Ok(())
}
