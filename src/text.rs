//! Turns raw PTY bytes into plain lines, and plain lines into compact text for the model.
use regex::Regex;
use std::sync::LazyLock;

/// A trailing partial line that looks like a program waiting for input.
pub static PROMPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(pass(word|phrase|code)?|token|otp)\b[^\n]*:\s*$|[\[(]y(es)?/no?[\])]|[>$#%?:]\s*$|^\s*\.\.\.\s*$").unwrap()
});

const MAX_LINES: usize = 20_000;
const MAX_LINE_CHARS: usize = 400;
const MAX_CUR_BYTES: usize = 64 * 1024;

#[derive(Default, Clone, Copy, PartialEq)]
enum Esc { #[default] None, Esc, Csi, Osc, OscEsc, Charset }

/// Plain-text view of a terminal stream: ANSI removed, `\r` redraws and
/// cursor-up redraws (progress bars, spinners) collapsed into their final state.
#[derive(Default)]
pub struct Lines {
    pub lines: Vec<String>,
    pub cur: String,
    /// Lines evicted from memory (still in the session log).
    pub dropped: usize,
    /// Bytes of plain text produced: what a plain shell tool would hand the model.
    pub plain_bytes: u64,
    esc: Esc,
    csi: String,
    cr: bool,
    utf8: Vec<u8>,
}

impl Lines {
    pub fn total(&self) -> usize { self.dropped + self.lines.len() }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.utf8.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.utf8);
        let text = match std::str::from_utf8(&buf) {
            Ok(s) => s.to_owned(),
            // Keep an incomplete trailing UTF-8 sequence for the next chunk.
            Err(e) if e.error_len().is_none() => {
                self.utf8 = buf[e.valid_up_to()..].to_vec();
                String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned()
            }
            Err(_) => String::from_utf8_lossy(&buf).into_owned(),
        };
        for c in text.chars() { self.push(c) }
        if self.lines.len() > MAX_LINES {
            let n = self.lines.len() - MAX_LINES;
            self.lines.drain(..n);
            self.dropped += n;
        }
    }

    fn push(&mut self, c: char) {
        match self.esc {
            Esc::Esc => {
                // `ESC ( B` and friends take one more byte (charset selection, used by `tput sgr0`).
                self.esc = match c { '[' => Esc::Csi, ']' | 'P' | '_' | '^' | 'X' => Esc::Osc, '(' | ')' | '*' | '+' | '#' | '%' => Esc::Charset, _ => Esc::None };
                self.csi.clear();
                return;
            }
            Esc::Csi => {
                if ('@'..='~').contains(&c) { self.esc = Esc::None; self.csi_final(c) } else { self.csi.push(c) }
                return;
            }
            Esc::Osc => { if c == '\x07' { self.esc = Esc::None } else if c == '\x1b' { self.esc = Esc::OscEsc } return }
            Esc::Charset => { self.esc = Esc::None; return }
            Esc::OscEsc => { self.esc = if c == '\\' { Esc::None } else { Esc::Osc }; return }
            Esc::None => {}
        }
        match c {
            '\x1b' => self.esc = Esc::Esc,
            '\n' => {
                let line = std::mem::take(&mut self.cur).trim_end().to_owned();
                self.plain_bytes += line.len() as u64 + 1;
                self.lines.push(line);
                self.cr = false;
            }
            '\r' => self.cr = true,
            '\x08' => { self.cur.pop(); }
            '\t' => self.put(' '),
            c if c.is_control() => {}
            c => self.put(c),
        }
    }

    fn csi_final(&mut self, c: char) {
        let n = self.csi.trim_start_matches('?').split(';').next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
        match c {
            // Cursor up: the program is about to redraw lines it already printed.
            'A' | 'F' => { let keep = self.lines.len().saturating_sub(n); self.lines.truncate(keep); self.cur.clear(); self.cr = false; }
            // Erase line.
            'K' if self.csi == "2" || (self.csi.is_empty() && self.cr) => { self.cur.clear(); self.cr = false; }
            // Cursor to column 1.
            'G' => self.cr = true,
            _ => {}
        }
    }

    fn put(&mut self, c: char) {
        if self.cr { self.cur.clear(); self.cr = false }
        // A single endless line (minified JSON, binary) must not grow without bound.
        if self.cur.len() < MAX_CUR_BYTES { self.cur.push(c) }
    }
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS { return line.to_owned() }
    let head: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{head}… (+{} chars)", line.chars().count() - MAX_LINE_CHARS)
}

/// Lines worth surfacing even when they fall in the hidden middle of long output.
static NOTABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(errors?|failed|failures?|fatal|panic(ked)?|exception|traceback)\b").unwrap()
});
const MAX_NOTABLE: usize = 10;

/// How much output one reply may carry.
#[derive(Clone, Copy)]
pub struct Budget { pub lines: usize, pub bytes: usize }

/// Collapses runs of identical lines and blank lines. Output within `budget` is returned whole;
/// beyond it, a head and a tail are kept, plus up to ten error-looking lines from the middle.
/// `first` is the absolute index of `lines[0]`.
pub fn compact(lines: &[String], first: usize, budget: Budget) -> String {
    let idx: Vec<usize> = (first..first + lines.len()).collect();
    compact_idx(lines, &idx, budget)
}

/// Like `compact`, but `idx[i]` is the absolute index of `lines[i]`, so line pointers stay
/// correct after some lines were filtered out.
pub fn compact_idx(lines: &[String], idx: &[usize], budget: Budget) -> String {
    let mut runs: Vec<(usize, &str, usize)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        match runs.last_mut() {
            Some((_, prev, n)) if *prev == l.as_str() => *n += 1,
            Some((_, prev, _)) if prev.is_empty() && l.is_empty() => {}
            _ => runs.push((idx[i], l.as_str(), 1)),
        }
    }
    while runs.first().is_some_and(|r| r.1.is_empty()) { runs.remove(0); }
    while runs.last().is_some_and(|r| r.1.is_empty()) { runs.pop(); }
    let render = |(_, l, n): &(usize, &str, usize)| if *n > 1 { format!("{} (×{n})", clip(l)) } else { clip(l) };
    let rendered: Vec<String> = runs.iter().map(render).collect();
    let size = |r: &[String]| r.iter().map(|l| l.len() + 1).sum::<usize>();
    let (max_lines, max_bytes) = (budget.lines.max(4), budget.bytes.max(256));
    if rendered.len() <= max_lines && size(&rendered) <= max_bytes { return rendered.join("\n") }
    // A quarter of the budget for the head, the rest for the tail, where results usually are.
    let (mut head, mut head_bytes) = (0, 0);
    while head < rendered.len() && head < max_lines / 4 && head_bytes + rendered[head].len() < max_bytes / 4 {
        head_bytes += rendered[head].len() + 1;
        head += 1;
    }
    let head = head.max(1);
    let (mut tail, mut tail_bytes) = (0, 0);
    while tail < rendered.len() - head && head + tail < max_lines && head_bytes + tail_bytes + rendered[rendered.len() - 1 - tail].len() < max_bytes {
        tail_bytes += rendered[rendered.len() - 1 - tail].len() + 1;
        tail += 1;
    }
    let hidden = head..rendered.len() - tail;
    if hidden.is_empty() { return rendered.join("\n") }
    let mut out = rendered[..head].to_vec();
    out.push(format!(
        "… {} lines hidden (read mode=all from={}, or grep) …",
        runs[hidden.clone()].iter().map(|r| r.2).sum::<usize>(),
        runs[hidden.start].0
    ));
    out.extend(runs[hidden.clone()].iter().zip(&rendered[hidden])
        .filter(|(r, _)| NOTABLE.is_match(r.1))
        .take(MAX_NOTABLE)
        .map(|(r, l)| format!("! {}: {l}", r.0)));
    out.extend_from_slice(&rendered[rendered.len() - tail..]);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lines(input: &[u8]) -> Lines { let mut l = Lines::default(); l.feed(input); l }

    #[test]
    fn strips_ansi_and_osc() {
        let l = lines(b"\x1b[1;31merror\x1b[0m: bad\n\x1b]0;title\x07ok\x1b(B\x1b[m\n");
        assert_eq!(l.lines, ["error: bad", "ok"]);
    }
    #[test]
    fn carriage_return_keeps_final_state() {
        let l = lines(b"10%\r50%\r100% done\r\n");
        assert_eq!(l.lines, ["100% done"]);
    }
    #[test]
    fn cursor_up_redraw_replaces_lines() {
        let l = lines(b"a: 1%\nb: 1%\n\x1b[2A\x1b[2Ka: 100%\n\x1b[2Kb: 100%\n");
        assert_eq!(l.lines, ["a: 100%", "b: 100%"]);
    }
    #[test]
    fn split_utf8_across_chunks() {
        let mut l = Lines::default();
        let s = "ñandú\n".as_bytes();
        l.feed(&s[..2]); l.feed(&s[2..]);
        assert_eq!(l.lines, ["ñandú"]);
    }
    #[test]
    fn compact_dedupes_and_truncates() {
        let v: Vec<String> = (0..10).map(|i| if i < 8 { "warn".into() } else { format!("l{i}") }).collect();
        assert_eq!(compact(&v, 0, Budget { lines: 20, bytes: 4000 }), "warn (×8)\nl8\nl9");
        let v: Vec<String> = (0..100).map(|i| format!("l{i}")).collect();
        let out = compact(&v, 0, Budget { lines: 8, bytes: usize::MAX });
        assert!(out.starts_with("l0\nl1\n… 92 lines hidden (read mode=all from=2"), "{out}");
        assert!(out.ends_with("l99"));
        assert_eq!(out.lines().count(), 9);
    }
    #[test]
    fn small_output_is_never_cut() {
        let v: Vec<String> = (0..50).map(|i| format!("commit {i}")).collect();
        assert!(!compact(&v, 0, Budget { lines: 100, bytes: 4000 }).contains("hidden"));
    }
    #[test]
    fn byte_budget_and_notable_lines() {
        let v: Vec<String> = (0..1000).map(|i| if i == 500 { "error[E0308]: mismatched types".into() } else { format!("line {i} {}", "x".repeat(60)) }).collect();
        let out = compact(&v, 0, Budget { lines: 100, bytes: 4000 });
        assert!(out.len() < 4600, "{}", out.len());
        assert!(out.contains("! 500: error[E0308]: mismatched types"), "{out}");
        assert!(out.ends_with(&v[999]));
    }
    #[test]
    fn prompt_detection() {
        for p in ["Password: ", "Continue? [Y/n] ", ">>> ", "... ", "user@host:~$ ", "Enter value:"] { assert!(PROMPT.is_match(p), "{p}") }
        assert!(!PROMPT.is_match("Compiling foo v0.1.0"));
        assert!(!PROMPT.is_match("Loading..."));
    }
}
