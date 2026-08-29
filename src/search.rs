//! Full-text search, delegated to whichever search tool the host already has.
//!
//! serve-md does not ship an index. Building one would mean holding every
//! document in memory and re-tokenising it on every change, to reimplement
//! — worse — a job `ripgrep` already does at memory bandwidth. So the server
//! shells out, in this order:
//!
//! 1. `rg`  — ripgrep. Structured `--json` output, so no line format to guess.
//! 2. `ag`  — the silver searcher.
//! 3. `grep` — POSIX, present almost everywhere else.
//!
//! If none is on `PATH`, search reports that and names all three. Nothing else
//! about the server is affected.
//!
//! # The rule that makes this safe
//!
//! The query reaches this module from an AI agent, over the network,
//! unauthenticated unless `--user` is set. Two invariants hold it:
//!
//! - **No shell, ever.** Every invocation is `Command::new(binary).arg(..)`,
//!   which `exec`s directly. No string is ever concatenated into a command
//!   line, so there is no quoting to get wrong and no metacharacter to
//!   escape. The pattern is passed after `-e` or `--` so a query beginning
//!   with `-` cannot be read as a flag.
//! - **Every hit is checked against the catalog before it is returned.** See
//!   [`keep_served_paths`]. The search tool's own exclusions are a
//!   performance measure and a second line of defence; the catalog membership
//!   test is the guarantee. It upholds the rule `scanner::is_forbidden_segment`
//!   already states — *a name the listing will not show must not be reachable*
//!   — which is what stops a search for `password` reporting a line of
//!   `.git/config` or `.env`.
//!
//! # References
//!
//! - ripgrep JSON output format:
//!   <https://docs.rs/grep-printer/latest/grep_printer/struct.JSON.html>
//! - `grep`, `-r`/`-n`/`-F`/`-e`, IEEE Std 1003.1-2017:
//!   <https://pubs.opengroup.org/onlinepubs/9699919799/utilities/grep.html>
//! - the silver searcher: <https://github.com/ggreer/the_silver_searcher>

use crate::catalog::Snapshot;
use crate::json;
use crate::scanner::SKIP_DIRS;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Ceilings on what one search may cost. A query arrives from the network, so
/// every one of these is a limit on an unauthenticated caller.
const MAX_QUERY_LEN: usize = 512;
/// Output past this is discarded and the child killed.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// Wall-clock budget for the child process.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Matches taken from any one file, so a single huge document cannot crowd
/// every other result out.
const MAX_PER_FILE: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Ripgrep,
    Ag,
    Grep,
}

impl Engine {
    pub fn binary(self) -> &'static str {
        match self {
            Engine::Ripgrep => "rg",
            Engine::Ag => "ag",
            Engine::Grep => "grep",
        }
    }
}

/// The order tried by [`detect`], and the list named in the error when none
/// is found.
pub const PREFERRED: [Engine; 3] = [Engine::Ripgrep, Engine::Ag, Engine::Grep];

/// One match: a served path and a 1-based line number.
///
/// Deliberately not the matched text. Every engine reports the line it found,
/// but in three different shapes, and none of them can report the heading the
/// line sits under. The caller re-reads the file instead and builds context
/// and headings itself, so the answer is identical whichever engine ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHit {
    pub rel: String,
    pub line: u64,
}

#[derive(Debug)]
pub enum Error {
    NoEngine,
    EmptyQuery,
    QueryTooLong,
    Spawn { engine: &'static str, msg: String },
    TimedOut { engine: &'static str },
    Failed { engine: &'static str, code: Option<i32> },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoEngine => {
                let names: Vec<&str> = PREFERRED.iter().map(|e| e.binary()).collect();
                write!(
                    f,
                    "no search tool found on PATH (looked for: {}). \
                     Install ripgrep (https://github.com/BurntSushi/ripgrep) to enable search.",
                    names.join(", ")
                )
            }
            Error::EmptyQuery => write!(f, "query must not be empty"),
            Error::QueryTooLong => write!(f, "query must be at most {MAX_QUERY_LEN} characters"),
            Error::Spawn { engine, msg } => write!(f, "could not run `{engine}`: {msg}"),
            Error::TimedOut { engine } => {
                write!(f, "`{engine}` exceeded its {}s budget", TIMEOUT.as_secs())
            }
            Error::Failed { engine, code } => match code {
                Some(c) => write!(f, "`{engine}` exited with status {c}"),
                None => write!(f, "`{engine}` was terminated"),
            },
        }
    }
}

/// The first available engine, in [`PREFERRED`] order.
///
/// Probed once at startup rather than per query, so a missing tool is
/// reported in the banner instead of at the first search.
pub fn detect() -> Option<Engine> {
    PREFERRED.into_iter().find(|e| probe(e.binary()))
}

fn probe(binary: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs `query` under `root` and returns the matches that the catalog serves.
///
/// `snap` is not optional and the filtering is not the caller's job: there is
/// deliberately no way to get an unfiltered result out of this module.
pub fn run(
    engine: Engine,
    root: &Path,
    snap: &Snapshot,
    query: &str,
    regex: bool,
) -> Result<Vec<RawHit>, Error> {
    if query.trim().is_empty() {
        return Err(Error::EmptyQuery);
    }
    if query.len() > MAX_QUERY_LEN {
        return Err(Error::QueryTooLong);
    }
    // Canonical and absolute, so the paths the child prints have a known
    // prefix to strip, and so a relative `--dir` cannot shift underneath us.
    let root_abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let out = capture(engine, &root_abs, query, regex)?;
    let hits = match engine {
        Engine::Ripgrep => parse_rg_json(&out),
        Engine::Ag | Engine::Grep => parse_colon_lines(&out),
    };
    Ok(keep_served_paths(hits, &root_abs, snap))
}

/// Builds the argument list. No shell is involved: each element becomes one
/// `argv` entry exactly as written.
fn args_for(engine: Engine, root: &Path, query: &str, regex: bool) -> Vec<String> {
    let root = root.to_string_lossy().to_string();
    let mut a: Vec<String> = Vec::new();
    match engine {
        Engine::Ripgrep => {
            a.push("--json".into());
            // Never read a user rc file: `RIPGREP_CONFIG_PATH` could otherwise
            // add `--follow` or `--hidden` and widen what search can reach.
            a.push("--no-config".into());
            a.push("--no-messages".into());
            // The scanner does not consult .gitignore, so search must not
            // either, or a served file would be unfindable.
            a.push("--no-ignore".into());
            a.push(format!("--max-count={MAX_PER_FILE}"));
            a.push("--ignore-case".into());
            if !regex {
                a.push("--fixed-strings".into());
            }
            for dir in SKIP_DIRS {
                a.push(format!("--glob=!{dir}/"));
            }
            a.push("-e".into());
            a.push(query.into());
            a.push("--".into());
            a.push(root);
        }
        Engine::Ag => {
            a.push("--nocolor".into());
            a.push("--noheading".into());
            a.push("--nogroup".into());
            a.push("--numbers".into());
            a.push("--silent".into());
            // ag reads .gitignore by default; the scanner does not.
            a.push("--skip-vcs-ignores".into());
            a.push(format!("--max-count={MAX_PER_FILE}"));
            a.push("--ignore-case".into());
            if !regex {
                a.push("--literal".into());
            }
            for dir in SKIP_DIRS {
                a.push("--ignore-dir".into());
                a.push((*dir).into());
            }
            // ag has no `-e`, so `--` is what keeps a query starting with `-`
            // from being read as a flag.
            a.push("--".into());
            a.push(query.into());
            a.push(root);
        }
        Engine::Grep => {
            // `-r`, not `-R`: `-R` follows symlinks, and the scanner does not
            // follow them either, so `-R` would let a link reach outside the
            // tree. The catalog filter would drop those hits anyway; this
            // keeps the process from reading them at all.
            a.push("-r".into());
            a.push("-n".into());
            a.push("-I".into());
            a.push("-i".into());
            a.push(format!("-m{MAX_PER_FILE}"));
            a.push(if regex { "-E".into() } else { "-F".into() });
            for dir in SKIP_DIRS {
                a.push(format!("--exclude-dir={dir}"));
            }
            a.push("-e".into());
            a.push(query.into());
            a.push("--".into());
            a.push(root);
        }
    }
    a
}

/// Spawns the child and reads its stdout under both a byte cap and a
/// wall-clock deadline.
///
/// The read happens on a second thread because `std` has no timed read: the
/// main thread waits on a channel instead, and kills the child if the deadline
/// passes, so a pathological search cannot pin a connection thread for longer
/// than [`TIMEOUT`].
fn capture(engine: Engine, root: &Path, query: &str, regex: bool) -> Result<String, Error> {
    let binary = engine.binary();
    let mut child = Command::new(binary)
        .args(args_for(engine, root, query, regex))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Discarded rather than inherited: a child's diagnostics must not
        // interleave with the server's own logging.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Spawn {
            engine: binary,
            msg: e.to_string(),
        })?;

    let mut stdout = child.stdout.take().ok_or(Error::Spawn {
        engine: binary,
        msg: "no stdout pipe".to_string(),
    })?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        // `take` caps the read itself, so a runaway child cannot make the
        // reader allocate without bound before anyone notices.
        let _ = stdout.by_ref().take(MAX_OUTPUT as u64).read_to_end(&mut buf);
        let truncated = buf.len() >= MAX_OUTPUT;
        let _ = tx.send((buf, truncated));
    });

    match rx.recv_timeout(TIMEOUT) {
        Ok((buf, truncated)) => {
            let status = child.wait().ok();
            // Exit status 1 means "no matches" for all three tools, which is
            // an answer, not a failure. Anything else is only trusted when the
            // output was complete.
            if !truncated {
                match status.and_then(|s| s.code()) {
                    Some(0 | 1) => {}
                    code => return Err(Error::Failed { engine: binary, code }),
                }
            }
            Ok(String::from_utf8_lossy(&buf).into_owned())
        }
        Err(_) => {
            kill(&mut child);
            Err(Error::TimedOut { engine: binary })
        }
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    // Reaped so the child does not linger as a zombie for the life of the
    // server process.
    let _ = child.wait();
}

/// Pulls matches out of ripgrep's newline-delimited JSON.
///
/// Only `type: "match"` records are of interest. A record whose path is
/// reported as `bytes` rather than `text` — a filename that is not valid
/// UTF-8 — is skipped: the catalog cannot hold such a name either.
fn parse_rg_json(out: &str) -> Vec<RawHit> {
    let mut hits = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = json::parse(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let Some(data) = v.get("data") else { continue };
        let Some(rel) = data.get("path").and_then(|p| p.get("text")).and_then(|t| t.as_str())
        else {
            continue;
        };
        let Some(line_no) = data.get("line_number").and_then(|n| n.as_i64()) else {
            continue;
        };
        if line_no > 0 {
            hits.push(RawHit {
                rel: rel.to_string(),
                line: line_no as u64,
            });
        }
    }
    hits
}

/// Pulls matches out of the `path:line:text` form that `ag --nogroup` and
/// `grep -n` share.
///
/// Split from the left on the *last* colon that precedes a run of digits
/// followed by a colon, because a path may legitimately contain a colon and
/// so may the matched text.
fn parse_colon_lines(out: &str) -> Vec<RawHit> {
    let mut hits = Vec::new();
    for line in out.lines() {
        let Some((rel, line_no)) = split_path_and_line(line) else {
            continue;
        };
        hits.push(RawHit {
            rel: rel.to_string(),
            line: line_no,
        });
    }
    hits
}

/// `docs/a.md:12:some text` -> `("docs/a.md", 12)`.
///
/// Scans left to right for the first `:<digits>:`, which is the earliest
/// position the line number can occupy. A path containing a colon followed by
/// digits would mis-split, but such a hit is then dropped by the catalog check
/// rather than misreported.
fn split_path_and_line(line: &str) -> Option<(&str, u64)> {
    let mut from = 0;
    while let Some(i) = line[from..].find(':') {
        let colon = from + i;
        let rest = &line[colon + 1..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest.as_bytes().get(digits.len()) == Some(&b':') {
            let n = digits.parse().ok()?;
            return Some((&line[..colon], n));
        }
        from = colon + 1;
    }
    None
}

/// Rewrites each hit's absolute path to a root-relative one and drops every
/// hit the catalog does not serve.
///
/// This is the security boundary of the module. A path that escaped the root,
/// a path under `.git`, `node_modules` or a dotfile, a file created after the
/// last catalog refresh — none of them are in the snapshot, so none of them
/// survive. Duplicate line numbers within a file are collapsed.
fn keep_served_paths(hits: Vec<RawHit>, root: &Path, snap: &Snapshot) -> Vec<RawHit> {
    let prefix = root.to_string_lossy().to_string();
    let mut out: Vec<RawHit> = Vec::with_capacity(hits.len());
    for hit in hits {
        let Some(rel) = to_rel(&hit.rel, &prefix) else {
            continue;
        };
        if !snap.contains(&rel) {
            continue;
        }
        let candidate = RawHit { rel, line: hit.line };
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Strips the served root from a path the child printed, normalising Windows
/// separators on the way, and refuses anything that did not start under it.
fn to_rel(printed: &str, root_prefix: &str) -> Option<String> {
    let normalized = printed.replace('\\', "/");
    let prefix = root_prefix.replace('\\', "/");
    let prefix = prefix.trim_end_matches('/');
    let rest = normalized.strip_prefix(prefix)?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::FileEntry;
    use std::time::SystemTime;

    fn snap(paths: &[&str]) -> Snapshot {
        let mut files: Vec<FileEntry> = paths
            .iter()
            .map(|p| FileEntry {
                rel: (*p).to_string(),
                size: 1,
                modified: SystemTime::UNIX_EPOCH,
            })
            .collect();
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        Snapshot { files }
    }

    #[test]
    fn parses_ripgrep_json() {
        let out = concat!(
            r#"{"type":"begin","data":{"path":{"text":"/root/a.md"}}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"/root/a.md"},"lines":{"text":"hi\n"},"line_number":3,"submatches":[]}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"/root/sub/b.md"},"lines":{"text":"yo\n"},"line_number":12,"submatches":[]}}"#,
            "\n",
            r#"{"type":"end","data":{"path":{"text":"/root/a.md"}}}"#,
            "\n",
            r#"{"type":"summary","data":{"elapsed_total":{"secs":0}}}"#,
        );
        assert_eq!(
            parse_rg_json(out),
            vec![
                RawHit { rel: "/root/a.md".into(), line: 3 },
                RawHit { rel: "/root/sub/b.md".into(), line: 12 },
            ]
        );
    }

    #[test]
    fn ripgrep_parser_survives_junk() {
        // A non-UTF-8 filename arrives as `bytes`, not `text`, and is skipped.
        let out = concat!(
            "not json at all\n",
            r#"{"type":"match","data":{"path":{"bytes":"3q2+7w=="},"line_number":1}}"#,
            "\n",
            r#"{"type":"match","data":{"path":{"text":"/root/a.md"}}}"#,
            "\n",
            "\n",
            r#"{"type":"match","data":{"path":{"text":"/root/ok.md"},"line_number":7}}"#,
        );
        assert_eq!(
            parse_rg_json(out),
            vec![RawHit { rel: "/root/ok.md".into(), line: 7 }]
        );
    }

    #[test]
    fn parses_colon_separated_output() {
        let out = "/root/a.md:3:some text\n/root/sub/b.md:12:more\n";
        assert_eq!(
            parse_colon_lines(out),
            vec![
                RawHit { rel: "/root/a.md".into(), line: 3 },
                RawHit { rel: "/root/sub/b.md".into(), line: 12 },
            ]
        );
    }

    #[test]
    fn colon_parser_handles_colons_in_the_matched_text() {
        // The text after the line number is full of colons and digits.
        let (path, line) = split_path_and_line("/root/a.md:9:see http://x:8080/y:1:2").unwrap();
        assert_eq!((path, line), ("/root/a.md", 9));
        assert!(split_path_and_line("no line number here").is_none());
        assert!(split_path_and_line("").is_none());
    }

    #[test]
    fn paths_are_made_relative_to_the_root() {
        assert_eq!(to_rel("/root/a.md", "/root").as_deref(), Some("a.md"));
        assert_eq!(to_rel("/root/sub/b.md", "/root/").as_deref(), Some("sub/b.md"));
        assert_eq!(to_rel("C:\\root\\a.md", "C:\\root").as_deref(), Some("a.md"));
        assert!(to_rel("/elsewhere/a.md", "/root").is_none());
        assert!(to_rel("/root", "/root").is_none());
    }

    #[test]
    fn hits_outside_the_catalog_are_dropped() {
        // The load-bearing test: whatever the engine reports, only files the
        // listing would show may come back.
        let served = snap(&["README.md", "docs/guide.md"]);
        let reported = vec![
            RawHit { rel: "/root/README.md".into(), line: 1 },
            RawHit { rel: "/root/.git/config".into(), line: 2 },
            RawHit { rel: "/root/.env".into(), line: 3 },
            RawHit { rel: "/root/node_modules/pkg/index.js".into(), line: 4 },
            RawHit { rel: "/root/docs/guide.md".into(), line: 5 },
            RawHit { rel: "/etc/passwd".into(), line: 6 },
            RawHit { rel: "/root/added-after-the-scan.md".into(), line: 7 },
        ];
        let kept = keep_served_paths(reported, Path::new("/root"), &served);
        assert_eq!(
            kept,
            vec![
                RawHit { rel: "README.md".into(), line: 1 },
                RawHit { rel: "docs/guide.md".into(), line: 5 },
            ]
        );
    }

    #[test]
    fn duplicate_hits_collapse() {
        let served = snap(&["a.md"]);
        let reported = vec![
            RawHit { rel: "/root/a.md".into(), line: 1 },
            RawHit { rel: "/root/a.md".into(), line: 1 },
            RawHit { rel: "/root/a.md".into(), line: 2 },
        ];
        let kept = keep_served_paths(reported, Path::new("/root"), &served);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn the_query_is_never_concatenated_into_a_command() {
        // A query full of shell metacharacters must appear as exactly one
        // argument, unaltered, and must come after `-e` or `--`.
        let nasty = "; rm -rf / $(whoami) `id` && echo";
        for engine in PREFERRED {
            let args = args_for(engine, Path::new("/root"), nasty, false);
            let at = args.iter().position(|a| a == nasty).unwrap_or_else(|| {
                panic!("{} did not pass the query through verbatim", engine.binary())
            });
            assert!(at > 0, "query must not be the first argument");
            assert!(
                args[at - 1] == "-e" || args[at - 1] == "--",
                "{}: query must follow -e or --, got {:?}",
                engine.binary(),
                args[at - 1]
            );
            assert_eq!(args.iter().filter(|a| *a == nasty).count(), 1);
        }
    }

    #[test]
    fn a_query_starting_with_a_dash_is_still_a_query() {
        for engine in PREFERRED {
            let args = args_for(engine, Path::new("/root"), "--version", false);
            let at = args.iter().position(|a| a == "--version").unwrap();
            assert!(args[at - 1] == "-e" || args[at - 1] == "--");
        }
    }

    #[test]
    fn literal_by_default_and_regex_only_on_request() {
        let literal = args_for(Engine::Ripgrep, Path::new("/r"), "a.*b", false);
        assert!(literal.iter().any(|a| a == "--fixed-strings"));
        let regex = args_for(Engine::Ripgrep, Path::new("/r"), "a.*b", true);
        assert!(!regex.iter().any(|a| a == "--fixed-strings"));

        assert!(args_for(Engine::Grep, Path::new("/r"), "x", false).iter().any(|a| a == "-F"));
        assert!(args_for(Engine::Grep, Path::new("/r"), "x", true).iter().any(|a| a == "-E"));
        assert!(args_for(Engine::Ag, Path::new("/r"), "x", false).iter().any(|a| a == "--literal"));
    }

    #[test]
    fn grep_does_not_follow_symlinks() {
        // `-R` would follow links back out of the served tree.
        let args = args_for(Engine::Grep, Path::new("/r"), "x", false);
        assert!(args.iter().any(|a| a == "-r"));
        assert!(!args.iter().any(|a| a == "-R"));
    }

    #[test]
    fn skipped_directories_are_excluded_by_every_engine() {
        for engine in PREFERRED {
            let args = args_for(engine, Path::new("/r"), "x", false).join(" ");
            for dir in SKIP_DIRS {
                assert!(args.contains(dir), "{} did not exclude {dir}", engine.binary());
            }
        }
    }

    #[test]
    fn queries_are_bounded() {
        let served = snap(&["a.md"]);
        let root = Path::new(".");
        assert!(matches!(
            run(Engine::Grep, root, &served, "   ", false),
            Err(Error::EmptyQuery)
        ));
        let long = "x".repeat(MAX_QUERY_LEN + 1);
        assert!(matches!(
            run(Engine::Grep, root, &served, &long, false),
            Err(Error::QueryTooLong)
        ));
    }

    #[test]
    fn the_no_engine_error_names_every_candidate() {
        let msg = Error::NoEngine.to_string();
        for engine in PREFERRED {
            assert!(msg.contains(engine.binary()));
        }
    }
}
