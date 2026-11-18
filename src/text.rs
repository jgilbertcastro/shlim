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
    /// Lowest line index the text was cut back to (by a cursor-up redraw) since the last read.
    /// Lines from there on were rewritten, so a reader positioned beyond it must come back.
    pub low: Option<usize>,
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
            'A' | 'F' => {
                let keep = self.lines.len().saturating_sub(n);
                // Erased lines were never output a plain shell would have kept: take them back out of the count.
                let erased: u64 = self.lines[keep..].iter().map(|l| l.len() as u64 + 1).sum();
                self.plain_bytes = self.plain_bytes.saturating_sub(erased);
                self.lines.truncate(keep);
                let at = self.total();
                self.low = Some(self.low.map_or(at, |l| l.min(at)));
                self.cur.clear();
                self.cr = false;
            }
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

/// Runs of three or more spaces inside a line (terminal padding, aligned columns) become two.
/// Leading indentation is kept. Used on what is sent to the agent, never on the stored text.
pub fn squeeze(line: &str) -> String {
    let body = line.trim_start_matches(' ');
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..line.len() - body.len()]);
    let mut run = 0;
    for c in body.chars() {
        if c == ' ' { run += 1; if run <= 2 { out.push(' ') } } else { run = 0; out.push(c) }
    }
    out
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS { return line.to_owned() }
    let head: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{head}… (+{} chars)", line.chars().count() - MAX_LINE_CHARS)
}

/// Lines worth surfacing even when they fall in the hidden middle of long output.
pub static NOTABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(errors?|failed|failures?|fatal|panic(ked)?|exception|traceback)\b").unwrap()
});
const MAX_NOTABLE: usize = 10;

/// "0 errors", "Failed: 0": mentions an error word but reports that all is well.
static BENIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(0|no|zero|without)\s+(errors?|failures?|failed|warnings?)\b|\b(errors?|failed|failures?|warnings?)\s*:\s*0\b|\b0\s+warning\(s\)").unwrap()
});

/// A line that reports a real problem (an error word, and not "0 errors").
pub fn is_alert(line: &str) -> bool { NOTABLE.is_match(line) && !BENIGN.is_match(line) }

static LOCATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[\w./\\:~@+-]+(?:\(\d+(?:,\d+)?\)|:\d+(?::\d+)?):?\s*").unwrap());
static PROJECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\[[^\]]*\.(?:csproj|sln|vcxproj|fsproj)\]\s*$").unwrap());

/// What an error says apart from where it happened: file, line, column and numbers removed, so
/// 35 copies of the same mistake in 35 files count as one kind of error.
pub fn signature(line: &str) -> String {
    let body = LOCATION.replace(line, "");
    let body = PROJECT.replace(&body, "");
    // A number becomes `#`, except right after a capital letter, where it is part of an error code (CS0029, E0308).
    let mut out = String::with_capacity(body.len());
    let mut prev = ' ';
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut run = String::from(c);
            while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) { run.push(d); chars.next(); }
            if prev.is_ascii_uppercase() { out.push_str(&run) } else { out.push('#') }
            prev = '0';
        } else {
            out.push(c);
            prev = c;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

const MAX_WARNINGS: usize = 5;

static WARNING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bwarn(ing)?s?\b").unwrap());

/// A warning line that is not an error and not an "0 warnings" summary.
pub fn is_warning(line: &str) -> bool { WARNING.is_match(line) && !BENIGN.is_match(line) && !is_alert(line) }

/// Lines picked by `pick`, one entry per kind (same `signature`): `{mark} {index}: {first line} (+N similar)`.
fn group_kinds(runs: &[(usize, &str, usize)], pick: fn(&str) -> bool, mark: &str, what: &str, max: usize) -> Vec<String> {
    let mut kinds: Vec<(usize, &str, usize, String)> = Vec::new(); // (first index, first line, count, signature)
    for r in runs.iter().filter(|r| pick(r.1)) {
        let sig = signature(r.1);
        match kinds.iter_mut().find(|k| k.3 == sig) {
            Some(k) => k.2 += r.2,
            None => kinds.push((r.0, r.1, r.2, sig)),
        }
    }
    let mut out: Vec<String> = kinds.iter().take(max).map(|(at, line, n, _)| {
        if *n > 1 { format!("{mark} {at}: {} (+{} similar)", clip(line), n - 1) } else { format!("{mark} {at}: {}", clip(line)) }
    }).collect();
    if kinds.len() > max {
        out.push(format!("{mark} … {} more kinds of {what} (read grep={what})", kinds.len() - max));
    }
    out
}

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
    // Problems hidden in the middle, one line per kind: where it first appeared and how many more.
    // Errors come first and get more room; warnings follow, marked with `~`.
    out.extend(group_kinds(&runs[hidden.clone()], is_alert, "!", "error", MAX_NOTABLE));
    out.extend(group_kinds(&runs[hidden], is_warning, "~", "warning", MAX_WARNINGS));
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
    fn redrawn_progress_counts_only_its_final_state() {
        // A docker-compose style block of 3 lines repainted 200 times.
        let mut input = Vec::new();
        for n in 0..200 {
            if n > 0 { input.extend_from_slice(b"\x1b[3A") }
            for name in ["web", "db", "cache"] { input.extend_from_slice(format!("\x1b[2K {name} Pulling {n}%\n").as_bytes()) }
        }
        let l = lines(&input);
        assert_eq!(l.lines.len(), 3);
        let final_bytes: u64 = l.lines.iter().map(|x| x.len() as u64 + 1).sum();
        assert_eq!(l.plain_bytes, final_bytes, "erased frames must not count as output");
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
    fn similar_errors_collapse_into_one_kind() {
        let v: Vec<String> = (0..2000).map(|i| match i {
            701 => "src/Other.cs(1,1): error CS0103: The name 'x' does not exist [/p/app.csproj]".into(),
            801 => "Build finished with 0 errors".into(),
            i if i % 2 == 0 && (100..1900).contains(&i) => format!("src/File{i}.cs({i},5): error CS0029: Cannot convert type 'string' to 'int' [/p/app.csproj]"),
            _ => format!("noise {i} {}", "x".repeat(30)),
        }).collect();
        let out = compact(&v, 0, Budget { lines: 40, bytes: 4000 });
        let bang: Vec<&str> = out.lines().filter(|l| l.starts_with("! ")).collect();
        assert_eq!(bang.len(), 2, "{out}");
        assert!(bang[0].contains("CS0029") && bang[0].contains("(+8") && bang[0].contains("similar"), "{}", bang[0]);
        assert!(bang[1].contains("CS0103") && !bang[1].contains("similar"), "{}", bang[1]);
        assert!(!out.contains("0 errors"), "benign line surfaced: {out}");
    }
    #[test]
    fn similar_warnings_collapse_after_the_errors() {
        let v: Vec<String> = (0..600).map(|i| match i {
            300 => "src/E.cs(1,1): error CS1002: ; expected [/p/app.csproj]".into(),
            301 => "    0 Warning(s)".into(),
            302 => "src/G.cs(9,9): warning CS8600: Converting null literal [/p/app.csproj]".into(),
            i if i % 3 == 0 => format!("src/F{i}.cs({i},5): warning CS0219: The variable 'u{i}' is assigned but its value is never used [/p/app.csproj]"),
            _ => format!("noise {i} {}", "x".repeat(30)),
        }).collect();
        let out = compact(&v, 0, Budget { lines: 40, bytes: 4000 });
        let marks: Vec<&str> = out.lines().filter(|l| l.starts_with("! ") || l.starts_with("~ ")).collect();
        assert_eq!(marks.len(), 3, "{out}");
        assert!(marks[0].starts_with("! 300") && !marks[0].contains("similar"), "{}", marks[0]);
        assert!(marks[1].starts_with("~ ") && marks[1].contains("CS0219") && marks[1].contains("similar"), "{}", marks[1]);
        assert!(marks[2].contains("CS8600") && !marks[2].contains("similar"), "{}", marks[2]);
        assert!(!out.contains("0 Warning(s)"), "benign summary surfaced: {out}");
    }
    #[test]
    fn warning_detection_ignores_all_clear_summaries() {
        for ok in ["    0 Warning(s)", "warnings: 0", "no warnings"] { assert!(!is_warning(ok), "{ok}") }
        for bad in ["src/a.cs(1,1): warning CS0219: x", "npm warn deprecated x", "WARNING: something"] { assert!(is_warning(bad), "{bad}") }
        assert!(!is_warning("error: boom with a warning attached"), "errors are not warnings");
    }
    #[test]
    fn signature_ignores_location_and_numbers() {
        let a = signature("src/A.cs(3,9): error CS0029: Cannot convert 'string' to 'int' [/p/a.csproj]");
        let b = signature("lib/deep/B.cs(120,1): error CS0029: Cannot convert 'string' to 'int' [/q/b.csproj]");
        assert_eq!(a, b);
        assert_ne!(a, signature("src/A.cs(3,9): error CS0103: The name 'x' does not exist"));
        assert_eq!(signature("w.cs(1,1): warning CS0219: variable 'u12' unused"), signature("w.cs(2,2): warning CS0219: variable 'u102' unused"));
        assert_ne!(signature("e.cs(1,1): error CS0103: same words"), signature("e.cs(1,1): error CS0104: same words"));
        assert_eq!(signature("src/a.ts:12:3: error TS2304: nope"), signature("src/b.ts:99:1: error TS2304: nope"));
    }
    #[test]
    fn alerts_skip_all_clear_lines() {
        for ok in ["0 Error(s)", "Failed:     0, Passed: 41", "no errors found", "errors: 0"] { assert!(!is_alert(ok), "{ok}") }
        for bad in ["error CS0029: x", "Build FAILED.", "Unhandled exception. System.X", "panic: boom"] { assert!(is_alert(bad), "{bad}") }
    }
    #[test]
    fn squeeze_keeps_indentation_and_short_gaps() {
        assert_eq!(squeeze("    a      b  c"), "    a  b  c");
        assert_eq!(squeeze("x"), "x");
        assert_eq!(squeeze(" => [2/5] RUN apk add                    8.4s"), " => [2/5] RUN apk add  8.4s");
    }
    #[test]
    fn prompt_detection() {
        for p in ["Password: ", "Continue? [Y/n] ", ">>> ", "... ", "user@host:~$ ", "Enter value:"] { assert!(PROMPT.is_match(p), "{p}") }
        assert!(!PROMPT.is_match("Compiling foo v0.1.0"));
        assert!(!PROMPT.is_match("Loading..."));
    }
}
