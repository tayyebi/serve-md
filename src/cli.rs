use std::env;
use std::path::PathBuf;

pub struct Config {
    pub host: String,
    pub port: u16,
    pub dir: PathBuf,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub no_open: bool,
    pub verbose: bool,
}

pub enum ParseOutcome {
    Run(Config),
    Help,
    Version,
}

pub fn help() -> String {
    format!(
        "serve-md {version}
A minimal web server that lists and renders Markdown files.

USAGE:
    serve-md [OPTIONS]

OPTIONS:
        --host <HOST>      Address to bind to            [default: 127.0.0.1]
        --port <PORT>      Port to listen on             [default: 8080]
        --dir <DIR>        Directory to serve            [default: .]
        --user <USER>      Require Basic auth username
        --pass <PASS>      Password for --user (or env SERVE_MD_PASSWORD)
        --no-open          Do not open a browser on startup
    -h, --help             Print help
    -V, --version          Print version

EXAMPLES:
    serve-md
    serve-md --host 0.0.0.0 --port 9000 --dir ./docs
    serve-md --user admin --pass secret
    curl -u admin:secret http://127.0.0.1:8080/view/README.md
",
        version = env!("CARGO_PKG_VERSION")
    )
}

pub fn parse(args: &[String]) -> Result<ParseOutcome, String> {
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8080;
    let mut dir = PathBuf::from(".");
    let mut user: Option<String> = None;
    let mut pass: Option<String> = None;
    let mut no_open = false;

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

    Ok(ParseOutcome::Run(Config {
        host,
        port,
        dir,
        user,
        pass,
        no_open,
    }))
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
