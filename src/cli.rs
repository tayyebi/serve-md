use crate::plugin;
use std::env;
use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    pub host: String,
    pub port: u16,
    pub dir: PathBuf,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub no_open: bool,
    pub verbose: bool,
    pub plugins: plugin::Set,
    /// Watch the tree for changes instead of scanning once at startup.
    pub fresh: bool,
    /// How often the `--fresh` watcher re-walks the tree.
    pub fresh_interval: Duration,
}

/// How often `--fresh` re-scans when no interval is given.
///
/// A second is well under the time it takes to alt-tab to a browser and
/// reload, so an edit appears to be picked up instantly, while still costing
/// one directory walk per second on a tree small enough to serve from memory.
const DEFAULT_FRESH_INTERVAL_MS: u64 = 1000;

/// A floor on `--fresh-interval`. Below this the watcher would spend more time
/// walking the tree than the server spends answering requests.
const MIN_FRESH_INTERVAL_MS: u64 = 50;

pub enum ParseOutcome {
    Run(Config),
    Help,
    Version,
}

pub fn help() -> String {
    let plugins = plugin::catalog().join("\n                           ");
    format!(
        "serve-md {version}
A minimal web server that lists and renders Markdown and HTML files.

USAGE:
    serve-md [OPTIONS]

OPTIONS:
        --host <HOST>      Address to bind to            [default: 127.0.0.1]
        --port <PORT>      Port to listen on             [default: 8080]
        --dir <DIR>        Directory to serve            [default: .]
        --user <USER>      Require Basic auth username
        --pass <PASS>      Password for --user (or env SERVE_MD_PASSWORD)
        --no-open          Do not open a browser on startup
        --verbose          Log each request to stdout
        --fresh            Watch the directory and pick up changes while running
                           (without it the file list is read once, at startup)
        --fresh-interval <MS>
                           How often --fresh re-scans      [default: {interval}]
        --plugin <NAME>    Enable a plugin (repeatable or comma-separated;
                           none are enabled by default)
    -h, --help             Print help
    -V, --version          Print version

PLUGINS:
    {plugins}

EXAMPLES:
    serve-md
    serve-md --host 0.0.0.0 --port 9000 --dir ./docs
    serve-md --plugin math --dir ./docs
    serve-md --plugins math,mermaid --dir ./docs
    serve-md --plugin webmcp --fresh --dir ./docs
    serve-md --user admin --pass secret
    curl -u admin:secret http://127.0.0.1:8080/README.md

AGENTS:
    With --plugin webmcp, the server also answers at:
      POST /mcp          Model Context Protocol (tools + resources)
      GET  /llms.txt     generated index of the documents, unless one exists
      GET  /llms-full.txt  every document, concatenated
    Search needs one of `rg`, `ag` or `grep` on PATH.
",
        version = env!("CARGO_PKG_VERSION"),
        interval = DEFAULT_FRESH_INTERVAL_MS
    )
}

pub fn parse(args: &[String]) -> Result<ParseOutcome, String> {
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8080;
    let mut dir = PathBuf::from(".");
    let mut user: Option<String> = None;
    let mut pass: Option<String> = None;
    let mut no_open = false;
    let mut verbose = false;
    let mut fresh = false;
    let mut fresh_interval_ms = DEFAULT_FRESH_INTERVAL_MS;
    let mut plugin_names: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            "-V" | "--version" => return Ok(ParseOutcome::Version),
            "--no-open" => {
                no_open = true;
                i += 1;
            }
            "--verbose" => {
                verbose = true;
                i += 1;
            }
            "--fresh" => {
                fresh = true;
                i += 1;
            }
            "--fresh-interval" => {
                fresh_interval_ms = interval(&value(args, &mut i, "--fresh-interval")?)?;
                i += 1;
            }
            "--host" => {
                host = value(args, &mut i, "--host")?;
                i += 1;
            }
            "--port" => {
                let v = value(args, &mut i, "--port")?;
                port = v
                    .parse()
                    .map_err(|_| format!("invalid port: {v}"))?;
                i += 1;
            }
            "--dir" => {
                dir = PathBuf::from(value(args, &mut i, "--dir")?);
                i += 1;
            }
            "--user" => {
                user = Some(value(args, &mut i, "--user")?);
                i += 1;
            }
            "--pass" => {
                pass = Some(value(args, &mut i, "--pass")?);
                i += 1;
            }
            "--plugin" | "--plugins" => {
                push_plugins(&mut plugin_names, &value(args, &mut i, arg)?);
                i += 1;
            }
            _ => {
                if let Some(v) = arg.strip_prefix("--host=") {
                    host = v.to_string();
                } else if let Some(v) = arg.strip_prefix("--port=") {
                    port = v.parse().map_err(|_| format!("invalid port: {v}"))?;
                } else if let Some(v) = arg.strip_prefix("--dir=") {
                    dir = PathBuf::from(v);
                } else if let Some(v) = arg.strip_prefix("--user=") {
                    user = Some(v.to_string());
                } else if let Some(v) = arg.strip_prefix("--pass=") {
                    pass = Some(v.to_string());
                } else if let Some(v) = arg.strip_prefix("--fresh-interval=") {
                    fresh_interval_ms = interval(v)?;
                } else if let Some(v) = arg
                    .strip_prefix("--plugin=")
                    .or_else(|| arg.strip_prefix("--plugins="))
                {
                    push_plugins(&mut plugin_names, v);
                } else {
                    return Err(format!("unknown argument: {arg}"));
                }
                i += 1;
            }
        }
    }

    let pass = match (user.as_ref(), pass) {
        (None, None) => None,
        (Some(_), Some(p)) => Some(p),
        (Some(_), None) => match env::var("SERVE_MD_PASSWORD") {
            Ok(p) => Some(p),
            Err(_) => return Err("--user requires --pass (or SERVE_MD_PASSWORD)".to_string()),
        },
        (None, Some(_)) => return Err("--pass requires --user".to_string()),
    };

    // Resolved here so an unknown name fails before the port is bound.
    let plugins = plugin::Set::resolve(&plugin_names)?;

    Ok(ParseOutcome::Run(Config {
        host,
        port,
        dir,
        user,
        pass,
        no_open,
        verbose,
        plugins,
        fresh,
        fresh_interval: Duration::from_millis(fresh_interval_ms),
    }))
}

/// Parses and floors a `--fresh-interval`. Rejected here rather than clamped
/// silently, so `--fresh-interval 0` is reported as a mistake instead of
/// quietly becoming something else.
fn interval(v: &str) -> Result<u64, String> {
    let ms: u64 = v
        .parse()
        .map_err(|_| format!("invalid --fresh-interval: {v}"))?;
    if ms < MIN_FRESH_INTERVAL_MS {
        return Err(format!(
            "--fresh-interval must be at least {MIN_FRESH_INTERVAL_MS}ms (got {ms})"
        ));
    }
    Ok(ms)
}

/// Accepts both `--plugin a --plugin b` and `--plugins a,b`.
fn push_plugins(names: &mut Vec<String>, value: &str) {
    for name in value.split(',') {
        let name = name.trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
    }
}

fn value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn run(list: &[&str]) -> Config {
        match parse(&args(list)).unwrap() {
            ParseOutcome::Run(c) => c,
            _ => panic!("expected run outcome"),
        }
    }

    #[test]
    fn defaults() {
        let c = run(&[]);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8080);
        assert_eq!(c.dir, PathBuf::from("."));
        assert!(c.user.is_none());
        assert!(c.pass.is_none());
        assert!(!c.no_open);
        assert!(c.plugins.is_empty(), "plugins are opt-in");
        assert!(!c.fresh, "the catalog is startup-only unless asked otherwise");
        assert_eq!(
            c.fresh_interval,
            Duration::from_millis(DEFAULT_FRESH_INTERVAL_MS)
        );
    }

    #[test]
    fn fresh_flags() {
        assert!(run(&["--fresh"]).fresh);
        assert_eq!(
            run(&["--fresh", "--fresh-interval", "250"]).fresh_interval,
            Duration::from_millis(250)
        );
        assert_eq!(
            run(&["--fresh-interval=2000"]).fresh_interval,
            Duration::from_millis(2000)
        );
    }

    #[test]
    fn a_pointless_fresh_interval_is_refused_rather_than_clamped() {
        assert!(parse(&args(&["--fresh-interval", "0"])).is_err());
        assert!(parse(&args(&["--fresh-interval", "10"])).is_err());
        assert!(parse(&args(&["--fresh-interval", "abc"])).is_err());
        assert!(parse(&args(&["--fresh-interval"])).is_err());
    }

    #[test]
    fn help_documents_the_agent_surface() {
        let h = help();
        assert!(h.contains("--fresh"));
        assert!(h.contains("POST /mcp"));
        assert!(h.contains("/llms.txt"));
        assert!(h.contains("webmcp"));
    }

    #[test]
    fn explicit_values() {
        let c = run(&[
            "--host", "0.0.0.0", "--port", "9000", "--dir", "docs", "--user", "u", "--pass",
            "p", "--no-open",
        ]);
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 9000);
        assert_eq!(c.dir, PathBuf::from("docs"));
        assert_eq!(c.user.as_deref(), Some("u"));
        assert_eq!(c.pass.as_deref(), Some("p"));
        assert!(c.no_open);
    }

    #[test]
    fn equals_form() {
        let c = run(&["--host=1.2.3.4", "--port=1234", "--dir=./x", "--user=alice", "--pass=w"]);
        assert_eq!(c.host, "1.2.3.4");
        assert_eq!(c.port, 1234);
        assert_eq!(c.dir, PathBuf::from("./x"));
        assert_eq!(c.user.as_deref(), Some("alice"));
    }

    #[test]
    fn help_and_version() {
        assert!(matches!(parse(&args(&["-h"])).unwrap(), ParseOutcome::Help));
        assert!(matches!(parse(&args(&["--help"])).unwrap(), ParseOutcome::Help));
        assert!(matches!(parse(&args(&["-V"])).unwrap(), ParseOutcome::Version));
        assert!(matches!(parse(&args(&["--version"])).unwrap(), ParseOutcome::Version));
    }

    #[test]
    fn errors() {
        assert!(parse(&args(&["--wat"])).is_err());
        assert!(parse(&args(&["--port", "abc"])).is_err());
        assert!(parse(&args(&["--port", "70000"])).is_err());
        assert!(parse(&args(&["--pass", "x"])).is_err());
        assert!(parse(&args(&["--host"])).is_err());
        assert!(parse(&args(&["--plugin", "nope"])).is_err());
        assert!(parse(&args(&["--plugin"])).is_err());
    }

    #[test]
    fn plugin_selection() {
        assert_eq!(run(&["--plugin", "math"]).plugins.names(), vec!["math"]);
        assert_eq!(run(&["--plugin=math"]).plugins.names(), vec!["math"]);
        // Repeating a name is harmless.
        assert_eq!(
            run(&["--plugin", "math", "--plugin", "math"]).plugins.names(),
            vec!["math"]
        );
    }

    #[test]
    fn plugins_accept_repeated_and_comma_separated_forms() {
        let expected = vec!["math", "mermaid"];
        for form in [
            vec!["--plugin", "math", "--plugin", "mermaid"],
            vec!["--plugins", "math,mermaid"],
            vec!["--plugins=math,mermaid"],
            vec!["--plugin=math", "--plugin=mermaid"],
            vec!["--plugins", "math, mermaid"],
        ] {
            assert_eq!(run(&form).plugins.names(), expected, "{form:?}");
        }
        assert!(parse(&args(&["--plugins", "math,nope"])).is_err());
    }

    #[test]
    fn help_lists_available_plugins() {
        let h = help();
        assert!(h.contains("--plugin <NAME>"));
        assert!(h.contains("math"));
    }

    #[test]
    fn user_with_env_pass() {
        env::set_var("SERVE_MD_PASSWORD", "s");
        let result = parse(&args(&["--user", "u"]));
        env::remove_var("SERVE_MD_PASSWORD");
        let c = match result.unwrap() {
            ParseOutcome::Run(c) => c,
            _ => panic!("expected run"),
        };
        assert_eq!(c.pass.as_deref(), Some("s"));
    }
}
