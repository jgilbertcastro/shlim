//! Per-tool noise filters: lines that never help an agent decide what to do next
//! (restore chatter, per-test "Passed" lines, docker layer progress, one line per compiled crate).
//! Filtering only affects what is *sent*; the full text stays available through `read mode=all`.
use crate::text::NOTABLE;
use regex::Regex;
use std::sync::LazyLock;

struct Rule {
    /// Matches the command line that started the session.
    cmd: Regex,
    /// Lines to drop entirely.
    drop: Regex,
}

fn rule(cmd: &str, drop: &str) -> Rule { Rule { cmd: Regex::new(cmd).unwrap(), drop: Regex::new(drop).unwrap() } }

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| vec![
    rule(
        r"\bdotnet\s+(build|test|restore|publish|run|watch|msbuild)\b",
        r"^\s*(Determining projects to restore\.\.\.|All projects are up-to-date for restore\.|Restored .+\.csproj \(in .+\)\.?|.+ -> .+\.(dll|exe)|\s*Passed [^\[]+\[[^\]]*\]|Build started.*|Test run for .+|Microsoft \(R\) .+|VSTest version.+|Starting test execution, please wait\.\.\.|A total of \d+ test files matched the specified pattern\.)$",
    ),
    rule(
        r"\b(npm|pnpm|yarn|bun)\s+(install|i|add|ci|update|up)\b",
        r"^\s*(Progress: resolved .*|\+{3,}-*|Lockfile is up to date.*|Already up[- ]to[- ]date|Done in [\d.]+m?s|Packages: [+-]\d+|npm (warn|WARN) (deprecated|EBADENGINE).*|npm notice.*|added \d+ packages?.* audited.*|[\d.]+ ?[kM]?B/s.*)$",
    ),
    rule(
        r"\bdocker(\s+compose|-compose)?\s+(build|up|pull|buildx)\b",
        r"^\s*(#\d+ (DONE|CACHED|sha256:|extracting|transferring|\[internal\]|resolve )\S*.*|[a-f0-9]{12}: (Pulling fs layer|Waiting|Verifying Checksum|Download complete|Pull complete|Already exists)|\S+ Pulling|.*Layer already exists)$",
    ),
    rule(
        r"\bgit\s+(clone|fetch|pull|push)\b",
        r"^\s*(remote: (Enumerating|Counting|Compressing) objects.*|Receiving objects:.*|Resolving deltas:.*|Counting objects:.*|Compressing objects:.*|Writing objects:.*|Unpacking objects:.*)$",
    ),
]);

static CARGO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bcargo\s+(build|test|check|run|clippy|install|doc)\b").unwrap());
static CARGO_STEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(Compiling|Downloaded|Downloading|Fresh|Checking|Documenting|Locking|Updating|Adding) (\S+)").unwrap());

/// Returns the kept lines together with their absolute indices, and how many lines were dropped.
pub fn filter(cmd: &str, lines: &[String], first: usize) -> (Vec<String>, Vec<usize>, usize) {
    let drop_re: Vec<&Rule> = RULES.iter().filter(|r| r.cmd.is_match(cmd)).collect();
    let cargo = CARGO.is_match(cmd);
    if drop_re.is_empty() && !cargo { return (lines.to_vec(), (first..first + lines.len()).collect(), 0) }
    let mut out = Vec::with_capacity(lines.len());
    let mut idx = Vec::with_capacity(lines.len());
    let mut dropped = 0;
    // Consecutive cargo progress lines collapse into one summary line.
    let mut steps: Option<(usize, usize, usize, String)> = None; // (first idx, count, ..., last name)
    let flush = |steps: &mut Option<(usize, usize, usize, String)>, out: &mut Vec<String>, idx: &mut Vec<usize>| {
        if let Some((at, n, _, last)) = steps.take() {
            out.push(if n == 1 { format!("(1 crate step: {last})") } else { format!("({n} crate steps, last: {last})") });
            idx.push(at);
        }
    };
    for (i, l) in lines.iter().enumerate() {
        let abs = first + i;
        // Safety net: a line that looks like an error is never routine, whatever the tool rules say.
        let notable = NOTABLE.is_match(l);
        if cargo {
            if let Some(c) = CARGO_STEP.captures(l).filter(|_| !notable) {
                let name = c[2].to_owned();
                match steps.as_mut() {
                    Some(s) => { s.1 += 1; s.3 = name; }
                    None => steps = Some((abs, 1, 0, name)),
                }
                dropped += 1;
                continue;
            }
            flush(&mut steps, &mut out, &mut idx);
            // `Finished`/`Running` lines are routine too, but short and useful: kept.
        }
        if !notable && drop_re.iter().any(|r| r.drop.is_match(l)) { dropped += 1; continue }
        out.push(l.clone());
        idx.push(abs);
    }
    flush(&mut steps, &mut out, &mut idx);
    (out, idx, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(s: &[&str]) -> Vec<String> { s.iter().map(|x| x.to_string()).collect() }

    #[test]
    fn error_looking_lines_are_never_dropped() {
        let l = v(&["  Passed Tests.Errors.Are.Reported [3 ms]", "   Compiling error-chain v0.12.4", "   Compiling serde v1"]);
        let (out, _, dropped) = filter("dotnet test", &l[..1], 0);
        assert_eq!((out.len(), dropped), (1, 0));
        let (out, _, _) = filter("cargo build", &l[1..], 0);
        assert_eq!(out, ["   Compiling error-chain v0.12.4", "(1 crate step: serde)"]);
    }

    /// Real output of real tools (tests/fixtures, first line is the command). The filters must save
    /// bytes without ever losing an error or a warning.
    #[test]
    fn real_outputs_keep_every_error_and_warning() {
        use crate::text::Lines;
        let fixtures = [
            include_str!("../tests/fixtures/dotnet_build_warn.txt"),
            include_str!("../tests/fixtures/dotnet_build_err.txt"),
            include_str!("../tests/fixtures/pnpm_install.txt"),
            include_str!("../tests/fixtures/cargo_build_err.txt"),
            include_str!("../tests/fixtures/docker_build_err.txt"),
            include_str!("../tests/fixtures/git_clone.txt"),
        ];
        let important = Regex::new(r"(?i)\b(warning|error)\b|\bFAILED\b|^\s*[0-9]+ (warning|error)").unwrap();
        for f in fixtures {
            let (cmd, body) = f.split_once('\n').unwrap();
            let cmd = cmd.trim_start_matches("$ ");
            let mut t = Lines::default();
            t.feed(body.as_bytes());
            let (kept, _, dropped) = filter(cmd, &t.lines, 0);
            for line in t.lines.iter().filter(|l| important.is_match(l)) {
                assert!(kept.contains(line), "`{cmd}` lost an important line: {line}");
            }
            assert!(dropped <= t.lines.len(), "`{cmd}` accounting");
        }
    }

    #[test]
    fn real_noisy_outputs_shrink() {
        use crate::text::Lines;
        for (f, max_pct) in [
            (include_str!("../tests/fixtures/docker_build_err.txt"), 85),
            (include_str!("../tests/fixtures/pnpm_install.txt"), 85),
            (include_str!("../tests/fixtures/dotnet_build_warn.txt"), 85),
        ] {
            let (cmd, body) = f.split_once('\n').unwrap();
            let mut t = Lines::default();
            t.feed(body.as_bytes());
            let (kept, _, _) = filter(cmd.trim_start_matches("$ "), &t.lines, 0);
            let (before, after) = (t.lines.join("\n").len(), kept.join("\n").len());
            assert!(after * 100 <= before * max_pct, "`{cmd}`: {before} -> {after} bytes");
        }
    }

    #[test]
    fn unknown_commands_are_untouched() {
        let l = v(&["a", "b"]);
        let (out, idx, dropped) = filter("ls -la", &l, 10);
        assert_eq!((out, idx, dropped), (l, vec![10, 11], 0));
    }
    #[test]
    fn dotnet_keeps_results_and_errors() {
        let l = v(&[
            "  Determining projects to restore...",
            "  All projects are up-to-date for restore.",
            "  Domain -> /src/bin/Debug/net10.0/Domain.dll",
            "Foo.cs(3,5): error CS1002: ; expected [/src/Foo.csproj]",
            "  Passed Tests.One [12 ms]",
            "Passed!  - Failed:     0, Passed:    41, Skipped:     0, Total:    41",
        ]);
        let (out, idx, dropped) = filter("cd backend && dotnet test --no-build", &l, 0);
        assert_eq!(dropped, 4);
        assert_eq!(out, ["Foo.cs(3,5): error CS1002: ; expected [/src/Foo.csproj]", "Passed!  - Failed:     0, Passed:    41, Skipped:     0, Total:    41"]);
        assert_eq!(idx, [3, 5]);
    }
    #[test]
    fn failed_tests_are_never_dropped() {
        let l = v(&["  Failed Tests.Two [3 ms]", "  Passed Tests.One [1 ms]"]);
        let (out, _, _) = filter("dotnet test", &l, 0);
        assert_eq!(out, ["  Failed Tests.Two [3 ms]"]);
    }
    #[test]
    fn cargo_steps_collapse() {
        let l = v(&["   Compiling a v1", "   Compiling b v2", "   Compiling c v3", "warning: unused", "    Finished dev in 1s"]);
        let (out, idx, dropped) = filter("cargo build", &l, 5);
        assert_eq!(out, ["(3 crate steps, last: c)", "warning: unused", "    Finished dev in 1s"]);
        assert_eq!(idx, [5, 8, 9]);
        assert_eq!(dropped, 3);
    }
    #[test]
    fn npm_progress_is_dropped_but_errors_stay() {
        let l = v(&["Progress: resolved 10, reused 0", "ERR_PNPM_FETCH_404 boom", "Done in 3.2s"]);
        let (out, _, _) = filter("pnpm install", &l, 0);
        assert_eq!(out, ["ERR_PNPM_FETCH_404 boom"]);
    }
}
