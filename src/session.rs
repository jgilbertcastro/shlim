//! One PTY-backed process, its plain-text view, its rendered screen and its token accounting.
use crate::noise;
use crate::text::{compact, compact_idx, is_alert, Budget, Lines, PROMPT};
use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use std::{
    fs::File,
    io::{Read, Write},
    num::NonZeroU16,
    sync::{mpsc, Arc, Condvar, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

pub const ROWS: u16 = 50;
pub const COLS: u16 = 200;
const PROMPT_QUIET: Duration = Duration::from_millis(300);
const EXIT_DRAIN: Duration = Duration::from_millis(200);
/// Finished commands faster than this, with exit 0 and nothing hidden, get no header.
const QUICK: Duration = Duration::from_secs(5);

pub enum Until { Auto, Exit, Quiet, Pattern(Regex) }

#[derive(PartialEq)]
enum Outcome { Exited, Prompt, Quiet, Matched, Timeout }

pub struct State {
    pub text: Lines,
    pub screen: vt100::Parser,
    pub exit: Option<(i32, Instant)>,
    eof: bool,
    pub last_out: Instant,
    /// Absolute line index the agent has read up to.
    cursor: usize,
    last_partial: String,
    /// A full-screen program drew since the last report; its byte stream is not line output.
    alt_seen: bool,
    /// Text the agent just typed; the terminal echoes it back and the agent doesn't need it.
    echo: Option<String>,
    /// Last time the agent or a human used the session.
    pub touched: Instant,
    /// Absolute line index up to which background errors have already been pointed out to the agent.
    alert_from: usize,
    exit_alerted: bool,
    pub raw_bytes: u64,
    /// Plain bytes of output the agent actually asked for (what a normal shell tool would have sent).
    pub read_bytes: u64,
    pub sent_bytes: u64,
    pub watchers: Vec<mpsc::Sender<Vec<u8>>>,
    log: Option<File>,
}

impl State {
    /// Where the agent's reading position really is: a redraw may have erased lines it already read.
    fn read_pos(&self) -> usize {
        self.cursor.min(self.text.low.unwrap_or(usize::MAX)).min(self.text.total())
    }
}

pub struct Session {
    pub id: u32,
    pub cmd: String,
    /// Exempt from the idle cleanup (quiet servers, tunnels, databases).
    pub keep: bool,
    pub started: Instant,
    st: Mutex<State>,
    cond: Condvar,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

/// Per-session options chosen by the client.
#[derive(Default)]
pub struct Opts {
    /// Full environment for the command; `None` inherits the daemon's.
    pub env: Option<Vec<(String, String)>>,
    pub keep: bool,
}

fn nz(v: u16) -> NonZeroU16 { NonZeroU16::new(v.max(1)).unwrap() }

impl Session {
    pub fn spawn(id: u32, cmd: &str, cwd: &str, log: Option<File>, opts: Opts) -> Result<Arc<Self>> {
        let pair = native_pty_system()
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        if cmd.trim().is_empty() { bail!("cmd is empty; for an interactive shell pass cmd=\"{shell}\"") }
        let mut b = CommandBuilder::new(&shell);
        b.args(["-c", cmd]);
        b.cwd(cwd);
        // The environment of the client that asked, not of whichever process happened to start the daemon.
        if let Some(env) = opts.env {
            b.env_clear();
            for (k, v) in env { b.env(k, v) }
        }
        b.env("TERM", "xterm-256color");
        // Pagers would trap the agent in an interactive screen.
        for k in ["PAGER", "GIT_PAGER", "MANPAGER"] { b.env(k, "cat") }
        b.env("SHLIM_SESSION", id.to_string());
        let mut child = pair.slave.spawn_command(b).with_context(|| format!("spawn {shell}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let now = Instant::now();
        let session = Arc::new(Session {
            id,
            cmd: cmd.to_owned(),
            keep: opts.keep,
            started: now,
            st: Mutex::new(State {
                text: Lines::default(),
                screen: vt100::Parser::new(nz(ROWS), nz(COLS), 0),
                exit: None,
                eof: false,
                last_out: now,
                cursor: 0,
                last_partial: String::new(),
                alt_seen: false,
                echo: None,
                touched: now,
                alert_from: 0,
                exit_alerted: false,
                raw_bytes: 0,
                read_bytes: 0,
                sent_bytes: 0,
                watchers: Vec::new(),
                log,
            }),
            cond: Condvar::new(),
            writer: Mutex::new(pair.master.take_writer()?),
            killer: Mutex::new(child.clone_killer()),
            master: Mutex::new(pair.master),
        });
        let s = session.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            while let Ok(n @ 1..) = reader.read(&mut buf) { s.on_output(&buf[..n]) }
            s.lock().eof = true;
            s.cond.notify_all();
        });
        let s = session.clone();
        thread::spawn(move || {
            let code = child.wait().map(|st| st.exit_code() as i32).unwrap_or(-1);
            s.lock().exit = Some((code, Instant::now()));
            s.cond.notify_all();
        });
        Ok(session)
    }

    pub fn lock(&self) -> MutexGuard<'_, State> { self.st.lock().unwrap_or_else(|e| e.into_inner()) }

    fn on_output(&self, bytes: &[u8]) {
        let mut st = self.lock();
        st.raw_bytes += bytes.len() as u64;
        st.text.feed(bytes);
        st.screen.process(bytes);
        if st.screen.screen().alternate_screen() { st.alt_seen = true }
        st.last_out = Instant::now();
        if let Some(log) = st.log.as_mut() { let _ = log.write_all(bytes); }
        st.watchers.retain(|w| w.send(bytes.to_vec()).is_ok());
        drop(st);
        self.cond.notify_all();
    }

    /// Input from the agent: remembered so its echo can be dropped from the reply.
    pub fn send(&self, text: &str, bytes: &[u8]) -> Result<()> {
        { let mut st = self.lock(); st.echo = Some(text.trim().to_owned()).filter(|t| !t.is_empty()); }
        self.write(bytes)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.lock().touched = Instant::now();
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.lock().unwrap_or_else(|e| e.into_inner())
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.lock().screen.set_size(nz(rows), nz(cols));
    }

    pub fn kill(&self) { let _ = self.killer.lock().unwrap_or_else(|e| e.into_inner()).kill(); }

    pub fn exited(&self) -> Option<i32> { self.lock().exit.map(|e| e.0) }

    /// Blocks until `until` is satisfied, the process exits, or `timeout` passes.
    fn wait<'a>(&'a self, mut st: MutexGuard<'a, State>, until: &Until, quiet: Duration, timeout: Duration) -> (MutexGuard<'a, State>, Outcome) {
        let start = Instant::now();
        let deadline = start + timeout;
        loop {
            if let Some((_, at)) = st.exit { if st.eof || at.elapsed() >= EXIT_DRAIN { return (st, Outcome::Exited) } }
            // Silence counts from the later of the last output and the start of this wait,
            // so input just sent gets time to produce a reply.
            let idle = st.last_out.max(start).elapsed();
            let prompt = idle >= PROMPT_QUIET && PROMPT.is_match(&st.text.cur);
            let hit = match until {
                Until::Exit => None,
                Until::Quiet => (idle >= quiet).then_some(Outcome::Quiet),
                Until::Auto => {
                    // An unfinished line that isn't a prompt is usually a progress bar
                    // (cargo, npm, docker), which can sit still for a while: wait longer.
                    let busy = !st.text.cur.trim().is_empty();
                    if prompt { Some(Outcome::Prompt) } else { (idle >= if busy { quiet * 5 } else { quiet }).then_some(Outcome::Quiet) }
                }
                Until::Pattern(re) => {
                    let from = st.read_pos().saturating_sub(st.text.dropped);
                    (st.text.lines[from.min(st.text.lines.len())..].iter().any(|l| re.is_match(l)) || re.is_match(&st.text.cur)).then_some(Outcome::Matched)
                }
            };
            if let Some(o) = hit { return (st, o) }
            let now = Instant::now();
            if timeout.is_zero() { return (st, if prompt { Outcome::Prompt } else { Outcome::Quiet }) }
            if now >= deadline { return (st, Outcome::Timeout) }
            st = self.cond.wait_timeout(st, (deadline - now).min(Duration::from_millis(100))).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn header(&self, st: &State, outcome: &Outcome) -> String {
        let secs = |d: Duration| format!("{:.1}s", d.as_secs_f64());
        let state = match (st.exit, outcome) {
            (Some((code, at)), _) => format!("exit {code} · {}", secs(at - self.started)),
            (None, Outcome::Prompt) => "waiting for input".into(),
            (None, Outcome::Timeout) => format!("running · timeout, {}", secs(self.started.elapsed())),
            (None, _) => format!("running · {}", secs(self.started.elapsed())),
        };
        format!("[#{} {state}]", self.id)
    }

    /// New output since the agent's last read; full-screen programs get their rendered screen.
    fn report(&self, st: &mut State, outcome: Outcome, budget: Budget) -> String {
        st.touched = Instant::now();
        let alt = st.screen.screen().alternate_screen() || std::mem::take(&mut st.alt_seen);
        let body = if alt {
            st.cursor = st.text.total();
            st.text.low = None;
            st.last_partial = st.text.cur.trim_end().to_owned();
            screen_text(&st.screen)
        } else {
            st.cursor = st.read_pos();
            st.text.low = None;
            let lost = st.text.dropped.saturating_sub(st.cursor);
            let start = st.cursor.max(st.text.dropped);
            let mut new: Vec<String> = st.text.lines[start - st.text.dropped..].to_vec();
            if let Some(echo) = st.echo.take() {
                if let Some(i) = new.iter().take(3).position(|l| l.trim_end().ends_with(&echo)) { new.remove(i); }
            }
            let partial = st.text.cur.trim_end().to_owned();
            if !partial.is_empty() && partial != st.last_partial { new.push(partial.clone()) }
            st.last_partial = partial;
            st.cursor = st.text.total();
            // What a plain shell tool would have handed the model for this read.
            st.read_bytes += new.iter().map(|l| l.len() as u64 + 1).sum::<u64>();
            let (kept, idx, dropped) = noise::filter(&self.cmd, &new, start);
            let mut b = compact_idx(&kept, &idx, budget);
            if dropped >= 5 { b.push_str(&format!("\n… {dropped} routine lines omitted (read mode=all to see them) …")) }
            if lost > 0 { b = format!("… {lost} older lines only in log …\n{b}") }
            b
        };
        if alt { st.read_bytes += body.len() as u64 }
        let quick_ok = outcome == Outcome::Exited
            && st.exit.is_some_and(|(code, at)| code == 0 && at - self.started < QUICK)
            && !body.contains(" lines hidden (");
        let out = match (quick_ok, body.is_empty()) {
            (true, true) => "(no output)".to_owned(),
            (true, false) => body,
            (false, true) => self.header(st, &outcome),
            (false, false) => format!("{}\n{body}", self.header(st, &outcome)),
        };
        st.sent_bytes += out.len() as u64;
        out
    }

    pub fn wait_report(&self, until: Until, quiet: Duration, timeout: Duration, budget: Budget) -> String {
        let (mut st, outcome) = self.wait(self.lock(), &until, quiet, timeout);
        self.report(&mut st, outcome, budget)
    }

    pub fn screen(&self) -> String {
        let mut st = self.lock();
        let body = screen_text(&st.screen);
        st.alert_from = st.text.total();
        let out = format!("{}\n{body}", self.header(&st, &Outcome::Quiet));
        st.read_bytes += body.len() as u64;
        st.sent_bytes += out.len() as u64;
        out
    }

    /// Lines by absolute index, optionally filtered by a regex, with line numbers.
    pub fn lines(&self, from: usize, grep: Option<&Regex>, limit: usize) -> String {
        let mut st = self.lock();
        let base = st.text.dropped;
        let picked: Vec<String> = st.text.lines.iter().enumerate()
            .map(|(i, l)| (base + i, l))
            .filter(|(i, l)| *i >= from && grep.is_none_or(|re| re.is_match(l)))
            .take(limit)
            .map(|(i, l)| format!("{i}: {l}"))
            .collect();
        let body = picked.join("\n");
        // Reading everything, or searching for errors, counts as having seen the errors.
        if picked.len() < limit && grep.is_none_or(|re| re.is_match("error") || re.is_match("failed")) { st.alert_from = st.text.total() }
        let out = format!("{} {} of {} lines\n{body}", self.header(&st, &Outcome::Quiet), picked.len(), st.text.total());
        st.read_bytes += body.len() as u64;
        st.sent_bytes += out.len() as u64;
        out
    }

    /// Something the agent has not seen yet in this session: new error lines since it last looked,
    /// or a non-zero exit it was not told about. Reported once, then considered seen.
    pub fn pending_alert(&self) -> Option<Alert> {
        let mut st = self.lock();
        if st.screen.screen().alternate_screen() { return None }
        let total = st.text.total();
        let from = st.read_pos().max(st.alert_from).max(st.text.dropped).min(total);
        let hits: Vec<&String> = st.text.lines[from - st.text.dropped..].iter().filter(|l| is_alert(l)).collect();
        let exit = st.exit.filter(|(code, at)| *code != 0 && *at > st.touched && !st.exit_alerted).map(|e| e.0);
        if hits.is_empty() && exit.is_none() { return None }
        let alert = Alert { errors: hits.len(), first: hits.first().map(|l| l.to_string()), exit };
        st.alert_from = total;
        if exit.is_some() { st.exit_alerted = true }
        Some(alert)
    }

    /// Plain lines from absolute index `from`, for a human following the session. Returns the
    /// next index to ask for and whether the process has finished. Not counted as agent traffic.
    pub fn tail(&self, from: usize) -> (usize, bool, Vec<String>) {
        let st = self.lock();
        let start = from.max(st.text.dropped).min(st.text.total());
        let mut lines: Vec<String> = st.text.lines[start - st.text.dropped..].to_vec();
        let next = st.text.total();
        let done = st.exit.is_some() && (st.eof || st.exit.is_some_and(|e| e.1.elapsed() >= EXIT_DRAIN));
        if done && !st.text.cur.trim().is_empty() { lines.push(st.text.cur.trim_end().to_owned()) }
        (next, done, lines)
    }
}

/// What changed in a session the agent is not looking at.
pub struct Alert { pub errors: usize, pub first: Option<String>, pub exit: Option<i32> }

fn screen_text(p: &vt100::Parser) -> String {
    let lines: Vec<String> = p.screen().contents().lines().map(|l| l.trim_end().to_owned()).collect();
    // Collapses runs like vim's `~` filler rows; never hides rows of a single screen.
    compact(&lines, 0, Budget { lines: lines.len(), bytes: usize::MAX })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cmd: &str) -> Arc<Session> { Session::spawn(1, cmd, "/tmp", None, Opts::default()).unwrap() }
    fn budget() -> Budget { Budget { lines: 100, bytes: 4000 } }
    const QUIET: Duration = Duration::from_millis(300);

    #[test]
    fn quick_command_returns_plain_output() {
        let s = run("printf 'a\\nb\\n'");
        let out = s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        assert_eq!(out, "a\nb");
    }

    #[test]
    fn failing_command_reports_exit_code() {
        let s = run("echo boom; exit 3");
        let out = s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        assert!(out.contains("exit 3") && out.contains("boom"), "{out}");
    }

    #[test]
    fn prompt_is_detected_without_waiting_for_timeout() {
        let s = run("printf 'Password: '; sleep 30");
        let t = Instant::now();
        let out = s.wait_report(Until::Auto, QUIET, Duration::from_secs(20), budget());
        assert!(out.contains("waiting for input"), "{out}");
        assert!(t.elapsed() < Duration::from_secs(5));
        s.kill();
    }

    #[test]
    fn unread_background_output_is_not_counted_as_read() {
        let s = run("echo first; sleep 1; for i in 1 2 3 4 5 6 7 8 9 10; do echo background-$i; done; sleep 30");
        s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        let after_first = s.lock().read_bytes;
        thread::sleep(Duration::from_millis(1800));
        let st = s.lock();
        assert!(st.text.plain_bytes > after_first, "background output was produced");
        assert_eq!(st.read_bytes, after_first, "but the agent never read it");
        drop(st);
        s.kill();
    }

    #[test]
    fn read_bytes_track_what_the_agent_consumed() {
        let s = run("for i in $(seq 1 300); do echo same; done");
        let out = s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        assert!(out.contains("same (×300)"), "{out}");
        let st = s.lock();
        assert_eq!(st.read_bytes, 300 * 5);
        assert!(st.sent_bytes < 100);
    }

    #[test]
    fn session_uses_the_environment_of_the_client() {
        let env = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned()), ("FROM_CLIENT".to_owned(), "yes".to_owned())];
        let s = Session::spawn(1, "echo \"[$FROM_CLIENT] [${DAEMON_ONLY:-unset}]\"", "/tmp", None, Opts { env: Some(env), keep: true }).unwrap();
        assert!(s.keep);
        let out = s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        assert_eq!(out, "[yes] [unset]");
    }

    #[test]
    fn background_errors_are_alerted_once_and_reading_clears_them() {
        let s = run("echo started; sleep 1; echo 'Build finished: 0 errors'; echo 'src/a.ts(3,1): error TS2304: nope'; echo 'src/b.ts(9,1): error TS2304: nope'; sleep 30");
        s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        assert!(s.pending_alert().is_none(), "nothing new yet");
        thread::sleep(Duration::from_millis(1800));
        let a = s.pending_alert().expect("two error lines arrived while the agent was away");
        assert_eq!(a.errors, 2);
        assert!(a.first.unwrap().contains("TS2304") && a.exit.is_none());
        assert!(s.pending_alert().is_none(), "reported only once");
        s.kill();
    }

    #[test]
    fn a_read_of_the_errors_means_no_alert() {
        let s = run("sleep 1; echo 'error: boom'; sleep 30");
        s.wait_report(Until::Auto, QUIET, Duration::from_secs(10), budget());
        thread::sleep(Duration::from_millis(1500));
        s.wait_report(Until::Auto, QUIET, Duration::from_secs(5), budget());
        assert!(s.pending_alert().is_none());
        s.kill();
    }

    #[test]
    fn unseen_failure_exit_is_alerted() {
        let s = run("sleep 1; exit 4");
        s.wait_report(Until::Auto, QUIET, Duration::from_secs(5), budget());
        thread::sleep(Duration::from_millis(1800));
        let a = s.pending_alert().expect("it failed after the agent looked");
        assert_eq!(a.exit, Some(4));
        assert!(s.pending_alert().is_none());
    }

    #[test]
    fn reading_after_a_redraw_erased_lines_does_not_panic() {
        // The agent reads 3 lines, then the program moves the cursor up over them.
        let s = run("printf 'a\\nb\\nc\\n'; sleep 1; printf '\\033[3A'; sleep 1; echo done");
        s.wait_report(Until::Auto, QUIET, Duration::from_millis(600), budget());
        thread::sleep(Duration::from_millis(1800));
        let out = s.wait_report(Until::Auto, QUIET, Duration::from_secs(5), budget());
        assert!(out.contains("done"), "{out}");
        let (_, _, tail) = s.tail(99);
        assert!(tail.len() <= 1);
    }

    #[test]
    fn tail_follows_new_lines() {
        let s = run("echo one; echo two");
        s.wait_report(Until::Exit, QUIET, Duration::from_secs(10), budget());
        let (next, done, lines) = s.tail(0);
        assert_eq!((next, done, lines), (2, true, vec!["one".to_owned(), "two".to_owned()]));
        assert!(s.tail(next).2.is_empty());
    }
}
