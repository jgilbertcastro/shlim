//! Talks to the daemon, starting it on first use.
use crate::daemon::{ensure_home, home, sock};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

pub fn connect() -> Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(sock()) { return Ok(s) }
    ensure_home()?;
    let log = OpenOptions::new().create(true).append(true).open(home().join("daemon.log"))?;
    Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()?;
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(30));
        if let Ok(s) = UnixStream::connect(sock()) { return Ok(s) }
    }
    bail!("shlim daemon did not start; see {}", home().join("daemon.log").display())
}

pub fn request(req: Value) -> Result<String> {
    let mut s = connect()?;
    writeln!(s, "{req}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    let resp: Value = serde_json::from_str(&line).map_err(|_| anyhow!("daemon closed the connection"))?;
    if resp["ok"].as_bool() == Some(true) { Ok(resp["text"].as_str().unwrap_or("").to_owned()) }
    else { bail!("{}", resp["error"].as_str().unwrap_or("unknown error")) }
}

/// Mirrors a session in this terminal. Typing goes to the process; Ctrl+\ detaches.
pub fn attach(id: u64) -> Result<()> {
    use crossterm::terminal;
    let (cols, rows) = terminal::size().unwrap_or((120, 40));
    let mut s = connect()?;
    writeln!(s, "{}", json!({"op": "attach", "id": id, "rows": rows, "cols": cols}))?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: Value = serde_json::from_str(&line)?;
    if resp["ok"].as_bool() != Some(true) { bail!("{}", resp["error"].as_str().unwrap_or("attach failed")) }
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b[2J\x1b[H{}", resp["screen"].as_str().unwrap_or(""))?;
    stdout.flush()?;
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        let mut stdout = std::io::stdout();
        while let Ok(n @ 1..) = reader.read(&mut buf) { let _ = stdout.write_all(&buf[..n]); let _ = stdout.flush(); }
        let _ = terminal::disable_raw_mode();
        println!("\r\n[shlim] session #{id} ended");
        std::process::exit(0);
    });
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1024];
    while let Ok(n @ 1..) = stdin.read(&mut buf) {
        let chunk = &buf[..n];
        if let Some(i) = chunk.iter().position(|&b| b == 0x1c) {
            let _ = s.write_all(&chunk[..i]);
            break;
        }
        if s.write_all(chunk).is_err() { break }
    }
    terminal::disable_raw_mode()?;
    println!("\r\n[shlim] detached from #{id}; the agent keeps the session");
    Ok(())
}
