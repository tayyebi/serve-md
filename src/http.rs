use crate::auth::Auth;
use crate::cli::Config;
use crate::encoding::percent_decode;
use crate::page;
use crate::scanner::{scan, FileEntry};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_HEADER: usize = 65_536;

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    ua: String,
}

struct Ctx {
    dir: PathBuf,
    files: Vec<FileEntry>,
    auth: Option<Auth>,
}

pub fn serve(cfg: Config) -> io::Result<()> {
    let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))?;
    serve_on(cfg, listener)
}

fn serve_on(cfg: Config, listener: TcpListener) -> io::Result<()> {
    let files = scan(&cfg.dir)?;
    let auth = match (cfg.user.as_ref(), cfg.pass.as_ref()) {
        (Some(u), Some(p)) => Some(Auth::new(u.clone(), p.clone())),
        _ => None,
    };
    let ctx = Arc::new(Ctx {
        dir: cfg.dir.clone(),
        files,
        auth,
    });

    let addr = listener.local_addr()?;
    let host = if cfg.host.is_empty() || cfg.host == "0.0.0.0" || cfg.host == "::" {
        "127.0.0.1".to_string()
    } else {
        cfg.host.clone()
    };
    let url = format!("http://{host}:{}/", addr.port());

    println!("serve-md {}", env!("CARGO_PKG_VERSION"));
    println!("serving: {}", cfg.dir.display());
    if let Some(u) = &cfg.user {
        println!("auth: Basic ({u})");
    }
    println!("  {url}");
    if !cfg.no_open {
        open_browser(&url);
    }

    for stream in listener.incoming() {
        let Ok(mut s) = stream else {
            continue;
        };
        let c = Arc::clone(&ctx);
        thread::spawn(move || {
            let _ = handle_connection(&mut s, &c);
        });
    }
    Ok(())
}

fn open_browser(url: &str) {
    let (cmd, args): (&str, Vec<&str>) = match std::env::consts::OS {
        "windows" => ("cmd", vec!["/C", "start", "", url]),
        "macos" => ("open", vec![url]),
        _ => ("xdg-open", vec![url]),
    };
    let _ = std::process::Command::new(cmd).args(&args).spawn();
}

fn handle_connection(stream: &mut TcpStream, ctx: &Ctx) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_nodelay(true);

    let Some(req) = read_request(stream)? else {
        return Ok(());
    };
    let is_head = req.method == "HEAD";
    if req.method != "GET" && req.method != "HEAD" {
        return respond(
            stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"405 Method Not Allowed\n",
            &[("Allow", "GET, HEAD")],
            is_head,
        );
    }

    let terminal = ua_is_terminal(&req.ua);
    let auth_ok = match &ctx.auth {
        Some(a) => a.check(&req.headers),
        None => true,
    };
    if !auth_ok {
        let extra = [("WWW-Authenticate", "Basic realm=\"serve-md\"")];
        if terminal {
            return respond(
                stream,
                401,
                "Unauthorized",
                "text/plain; charset=utf-8",
                b"401 Unauthorized\n",
                &extra,
                is_head,
            );
        }
        let body = page::unauthorized_html();
        return respond(
            stream,
            401,
            "Unauthorized",
            "text/html; charset=utf-8",
            body.as_bytes(),
            &extra,
            is_head,
        );
    }

    route(ctx, stream, &req, terminal, is_head)
}

fn route(
    ctx: &Ctx,
    stream: &mut TcpStream,
    req: &Request,
    terminal: bool,
    is_head: bool,
) -> io::Result<()> {
    let target_path = req.target.split('?').next().unwrap_or("/");
    let decoded = match percent_decode(target_path) {
        Ok(d) => d,
        Err(_) => {
            return respond(
                stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                b"400 Bad Request\n",
                &[],
                is_head,
            );
        }
    };

    if decoded == "/" {
        return listing(ctx, stream, terminal, is_head);
    }
    if let Some(rel) = decoded.strip_prefix("/view/").filter(|r| !r.is_empty()) {
        return view(ctx, stream, rel, terminal, is_head);
    }
    if let Some(rel) = decoded.strip_prefix("/raw/").filter(|r| !r.is_empty()) {
        return raw(ctx, stream, rel, is_head);
    }
    not_found(stream, terminal, is_head)
}

fn listing(ctx: &Ctx, stream: &mut TcpStream, terminal: bool, is_head: bool) -> io::Result<()> {
    if terminal {
        let body = page::listing_plain(&ctx.files, &ctx.dir);
        respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            body.as_bytes(),
            &[],
            is_head,
        )
    } else {
        let body = page::listing_html(&ctx.files, &ctx.dir);
        respond(
            stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            body.as_bytes(),
            &[],
            is_head,
        )
    }
}

fn view(
    ctx: &Ctx,
    stream: &mut TcpStream,
    rel: &str,
    terminal: bool,
    is_head: bool,
) -> io::Result<()> {
    let Some(full) = resolve_rel(&ctx.dir, rel) else {
        return not_found(stream, terminal, is_head);
    };
    let raw = match fs::read_to_string(&full) {
        Ok(r) => r,
        Err(_) => return not_found(stream, terminal, is_head),
    };
    if terminal {
        respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            raw.as_bytes(),
            &[],
            is_head,
        )
    } else {
        let html = page::render_markdown(&raw);
        let body = page::view_html(rel, &html, &ctx.dir, ctx.files.len());
        respond(
            stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            body.as_bytes(),
            &[],
            is_head,
        )
    }
}

fn raw(ctx: &Ctx, stream: &mut TcpStream, rel: &str, is_head: bool) -> io::Result<()> {
    let Some(full) = resolve_rel(&ctx.dir, rel) else {
        return not_found(stream, true, is_head);
    };
    let raw = match fs::read_to_string(&full) {
        Ok(r) => r,
        Err(_) => return not_found(stream, true, is_head),
    };
    respond(
        stream,
        200,
        "OK",
        "text/markdown; charset=utf-8",
        raw.as_bytes(),
        &[],
        is_head,
    )
}

fn not_found(stream: &mut TcpStream, terminal: bool, is_head: bool) -> io::Result<()> {
    if terminal {
        respond(
            stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"404 Not Found\n",
            &[],
            is_head,
        )
    } else {
        let body = page::not_found_html();
        respond(
            stream,
            404,
            "Not Found",
            "text/html; charset=utf-8",
            body.as_bytes(),
            &[],
            is_head,
        )
    }
}

fn resolve_rel(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return None;
    }
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
    }
    let root_c = root.canonicalize().ok()?;
    let candidate = root_c.join(rel);
    let cand_c = candidate.canonicalize().ok()?;
    if !cand_c.starts_with(&root_c) {
        return None;
    }
    let meta = fs::metadata(&cand_c).ok()?;
    if !meta.is_file() {
        return None;
    }
    let is_md = cand_c
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false);
    if !is_md {
        return None;
    }
    Some(cand_c)
}

fn ua_is_terminal(ua: &str) -> bool {
    let u = ua.to_ascii_lowercase();
    u.contains("curl") || u.contains("wget")
}

fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
        if has_header_end(&buf) {
            break;
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed request"))?;
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some(ci) = line.find(':') {
            headers.push((line[..ci].trim().to_string(), line[ci + 1..].trim().to_string()));
        }
    }
    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    Ok(Some(Request {
        method,
        target,
        headers,
        ua,
    }))
}

fn has_header_end(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n")
}

fn verbose_log(req: &Request, terminal: bool, ctx: &Ctx) {
    if !ctx.verbose {
        return;
    }
    let mut line = String::new();
    line.push_str(&format!("{} {} ", req.method, req.target));
    let wrapped = wrap_text(&line, 80);
    println!("[verbose] request: {}", wrapped);
    for (k, v) in &req.headers {
        println!("[verbose]   {}: {}", k, v);
    }
    println!("[verbose]   user-agent: {}", req.ua);
    println!("[verbose]   terminal: {}", terminal);
    println!("[verbose]   auth: {}", ctx.auth.is_some());
}

fn wrap_text(s: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.len() + 1 + word.len() <= max_width {
            line.push_str(" ");
            line.push_str(word);
        } else {
            line.push_str("\n");
            result.push_str(&line);
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        result.push_str(&line);
    }
    result
}

fn write_head(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    ctype: &str,
    len: usize,
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\n\
         Connection: close\r\nX-Content-Type-Options: nosniff\r\n"
    );
    for (k, v) in extra {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    ctype: &str,
    body: &[u8],
    extra: &[(&str, &str)],
    is_head: bool,
) -> io::Result<()> {
    write_head(stream, status, reason, ctype, body.len(), extra)?;
    if !is_head {
        stream.write_all(body)?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-http-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn setup() -> PathBuf {
        let dir = tmp_dir("smoke");
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::write(dir.join("a.md"), "# Alpha\n\nHello **world**.\n").unwrap();
        fs::write(dir.join("docs").join("b.md"), "# Beta\n").unwrap();
        fs::write(dir.join("ignore.txt"), "not markdown").unwrap();
        dir
    }

    fn start_server(dir: PathBuf, user: Option<&str>, pass: Option<&str>) -> SocketAddr {
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 0,
            dir,
            user: user.map(String::from),
            pass: pass.map(String::from),
            no_open: true,
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let _ = serve_on(cfg, listener);
        });
        std::thread::sleep(Duration::from_millis(30));
        addr
    }

    fn http_get(
        addr: SocketAddr,
        path: &str,
        ua: &str,
        auth: Option<&str>,
    ) -> (u16, String, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: {ua}\r\n");
        if let Some(a) = auth {
            req.push_str(&format!("Authorization: Basic {a}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        let status = resp
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("000")
            .parse()
            .unwrap_or(0);
        let ct = resp
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-type"))
            .unwrap_or("")
            .to_string();
        (status, resp, ct)
    }

    #[test]
    fn listing_and_views() {
        let addr = start_server(setup(), None, None);
        let (status, body, ct) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("a.md"));
        assert!(body.contains("docs/b.md"));
        assert!(!body.contains("ignore.txt"));

        let (status, body, _) = http_get(addr, "/view/a.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("<h1>Alpha</h1>"));

        let (status, body, _) = http_get(addr, "/raw/a.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("# Alpha"));

        let (status, _, _) = http_get(addr, "/view/missing.md", "test-agent", None);
        assert_eq!(status, 404);
    }

    #[test]
    fn terminal_user_agent() {
        let addr = start_server(setup(), None, None);
        let (status, body, ct) = http_get(addr, "/", "curl/8.7.1", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/plain"));
        assert!(body.contains("a.md"));
        assert!(body.contains("docs/b.md"));

        let (status, body, _) = http_get(addr, "/view/a.md", "Wget/1.21.4", None);
        assert_eq!(status, 200);
        assert!(body.contains("# Alpha"));
    }

    #[test]
    fn traversal_rejected() {
        let addr = start_server(setup(), None, None);
        for path in [
            "/view/..%2F..%2Fetc%2Fpasswd",
            "/view/../a.md",
            "/raw/../../etc/passwd",
            "/view/a.md%00",
            "/view/a.md\\..\\x",
        ] {
            let (status, _, _) = http_get(addr, path, "curl/8.7.1", None);
            assert_eq!(status, 404, "path: {path}");
        }
    }

    #[test]
    fn basic_auth() {
        let addr = start_server(setup(), Some("u"), Some("p"));
        let (status, _, _) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 401);
        let (status, body, _) = http_get(addr, "/", "test-agent", Some("dTpw"));
        assert_eq!(status, 200);
        assert!(body.contains("a.md"));
        let (status, _, _) = http_get(addr, "/", "test-agent", Some("b2Fr"));
        assert_eq!(status, 401);
    }

    #[test]
    fn head_request() {
        let addr = start_server(setup(), None, None);
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"HEAD / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        assert!(resp.starts_with("HTTP/1.1 200"));
    }
}
