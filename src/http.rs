use crate::auth::Auth;
use crate::cli::Config;
use crate::encoding::{percent_decode, percent_encode_path};
use crate::mime;
use crate::page;
use crate::plugin;
use crate::render;
use crate::scanner::{scan, FileEntry, FileKind};
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
    accept: String,
}

struct Ctx {
    dir: PathBuf,
    files: Vec<FileEntry>,
    auth: Option<Auth>,
    verbose: bool,
    plugins: plugin::Set,
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
        verbose: cfg.verbose,
        plugins: cfg.plugins,
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
    if !ctx.plugins.is_empty() {
        println!("plugins: {}", ctx.plugins.names().join(", "));
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
    verbose_log(&req, terminal, ctx);
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
    let (target_path, query) = match req.target.split_once('?') {
        Some((path, q)) => (path, Some(q)),
        None => (req.target.as_str(), None),
    };
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
    if decoded.starts_with('/') {
        let canonical = canonical_path(&ctx.dir, &decoded);
        if canonical != decoded {
            return redirect(stream, &canonical, query, is_head);
        }
    }
    let rel = decoded.strip_prefix('/').unwrap_or(&decoded);

    match resolve(&ctx.dir, rel) {
        Resolved::File(full, kind) => serve_file(ctx, stream, &full, kind, terminal, &req.accept, is_head),
        Resolved::Listing => listing(ctx, stream, terminal, is_head),
        Resolved::NotFound => not_found(stream, terminal, is_head),
    }
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

/// Serves an already-resolved file, negotiating format for `Markdown`/`Html`
/// (`Accept: text/markdown` -> source, `Accept: text/plain` or a terminal
/// client -> reader-friendly text, else rendered HTML) and streaming
/// `Static` files as-is with a guessed MIME type.
fn serve_file(
    ctx: &Ctx,
    stream: &mut TcpStream,
    full: &Path,
    kind: FileKind,
    terminal: bool,
    accept: &str,
    is_head: bool,
) -> io::Result<()> {
    if kind == FileKind::Static {
        let bytes = match fs::read(full) {
            Ok(b) => b,
            Err(_) => return not_found(stream, terminal, is_head),
        };
        return respond(stream, 200, "OK", mime::guess(full), &bytes, &[], is_head);
    }

    let src = match fs::read_to_string(full) {
        Ok(r) => r,
        Err(_) => return not_found(stream, terminal, is_head),
    };
    let rel = display_rel(&ctx.dir, full);

    if accept_wants(accept, "text/markdown") {
        let body = render::to_markdown(kind, &src);
        return respond(
            stream,
            200,
            "OK",
            "text/markdown; charset=utf-8",
            body.as_bytes(),
            &[],
            is_head,
        );
    }
    if terminal || accept_wants(accept, "text/plain") {
        let body = render::to_text(kind, &src, &ctx.plugins);
        return respond(
            stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            body.as_bytes(),
            &[],
            is_head,
        );
    }

    let rendered = render::to_html(kind, &src, &ctx.plugins);
    let body = match kind {
        FileKind::Markdown => page::view_html(
            &rel,
            &rendered.html,
            &rendered.head,
            &ctx.dir,
            ctx.files.len(),
        ),
        _ => rendered.html,
    };
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

fn display_rel(root: &Path, full: &Path) -> String {
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let rel = full.strip_prefix(&root_c).unwrap_or(full);
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&comp.as_os_str().to_string_lossy());
    }
    out
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

const INDEX_CANDIDATES: &[&str] = &["index.html", "index.md"];

/// The canonical spelling of a request path: runs of slashes collapsed to
/// one, the trailing slash dropped (the root itself stays `/`), and a
/// trailing `index.html`/`index.md` segment suppressed when the shorter path
/// serves the very same file. Anything spelled differently is redirected
/// here, so a document has exactly one URL.
fn canonical_path(root: &Path, path: &str) -> String {
    let collapsed = collapse_slashes(path);
    match parent_of_index(&collapsed) {
        Some(parent) if same_file(root, &collapsed, &parent) => parent,
        _ => collapsed,
    }
}

/// `/a//b/` -> `/a/b`; `/`, `//` and `///` all -> `/`.
fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    out.push('/');
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if out.len() > 1 {
            out.push('/');
        }
        out.push_str(seg);
    }
    out
}

/// The directory a slash-collapsed path names, when its last segment is an
/// index file: `/docs/index.md` -> `/docs`, `/index.html` -> `/`.
fn parent_of_index(path: &str) -> Option<String> {
    let cut = path.rfind('/')?;
    let name = &path[cut + 1..];
    if !INDEX_CANDIDATES.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return None;
    }
    Some(path[..cut.max(1)].to_string())
}

/// Whether two paths resolve to the same file on disk — the test for an
/// `index.*` segment being redundant. An `index.md` shadowed by an
/// `index.html` beside it is not redundant, and keeps its explicit URL.
fn same_file(root: &Path, a: &str, b: &str) -> bool {
    let a_rel = a.strip_prefix('/').unwrap_or(a);
    let b_rel = b.strip_prefix('/').unwrap_or(b);
    match (resolve(root, a_rel), resolve(root, b_rel)) {
        (Resolved::File(x, _), Resolved::File(y, _)) => x == y,
        _ => false,
    }
}

/// Points a client at the canonical spelling of what it asked for, keeping
/// the query string. `path` is decoded, so it is re-encoded for the header;
/// it always starts with a single slash, which keeps the target same-origin.
fn redirect(
    stream: &mut TcpStream,
    path: &str,
    query: Option<&str>,
    is_head: bool,
) -> io::Result<()> {
    let mut location = percent_encode_path(path);
    if let Some(q) = query {
        location.push('?');
        location.push_str(q);
    }
    let body = format!("301 Moved Permanently\n{location}\n");
    respond(
        stream,
        301,
        "Moved Permanently",
        "text/plain; charset=utf-8",
        body.as_bytes(),
        &[("Location", location.as_str())],
        is_head,
    )
}

enum Resolved {
    File(PathBuf, FileKind),
    Listing,
    NotFound,
}

/// Resolves a percent-decoded, leading-slash-stripped request path to a file
/// under `root`. A path that names a directory (including the served root
/// itself) falls back to `index.html`, then `index.md`, inside it; if
/// neither exists, the caller should show the full-tree listing page.
fn resolve(root: &Path, rel: &str) -> Resolved {
    let trimmed = rel.trim_end_matches('/');
    if trimmed.contains('\\') {
        return Resolved::NotFound;
    }
    if !trimmed.is_empty() {
        for seg in trimmed.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Resolved::NotFound;
            }
        }
    }
    let Ok(root_c) = root.canonicalize() else {
        return Resolved::NotFound;
    };
    let candidate = if trimmed.is_empty() {
        root_c.clone()
    } else {
        root_c.join(trimmed)
    };
    let Ok(cand_c) = candidate.canonicalize() else {
        return Resolved::NotFound;
    };
    if !cand_c.starts_with(&root_c) {
        return Resolved::NotFound;
    }
    let Ok(meta) = fs::metadata(&cand_c) else {
        return Resolved::NotFound;
    };
    if meta.is_file() {
        let kind = FileKind::from_path(&cand_c);
        return Resolved::File(cand_c, kind);
    }
    if meta.is_dir() {
        for name in INDEX_CANDIDATES {
            let idx = cand_c.join(name);
            let valid = idx.canonicalize().ok().filter(|idx_c| {
                idx_c.starts_with(&root_c) && fs::metadata(idx_c).map(|m| m.is_file()).unwrap_or(false)
            });
            if let Some(idx_c) = valid {
                let kind = FileKind::from_path(&idx_c);
                return Resolved::File(idx_c, kind);
            }
        }
        return Resolved::Listing;
    }
    Resolved::NotFound
}

fn ua_is_terminal(ua: &str) -> bool {
    let u = ua.to_ascii_lowercase();
    u.contains("curl") || u.contains("wget")
}

fn accept_wants(accept: &str, wanted: &str) -> bool {
    accept
        .split(',')
        .map(|part| part.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .any(|t| t == wanted || (wanted == "text/markdown" && t == "text/x-markdown"))
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
    let accept = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    Ok(Some(Request {
        method,
        target,
        headers,
        ua,
        accept,
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
            line.push(' ');
            line.push_str(word);
        } else {
            line.push('\n');
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
        fs::write(dir.join("page.html"), "<h1>Gamma</h1><p>Hello <b>html</b>.</p>").unwrap();
        dir
    }

    fn start_server(dir: PathBuf, user: Option<&str>, pass: Option<&str>) -> SocketAddr {
        start_server_with(dir, user, pass, &[])
    }

    fn start_server_with(
        dir: PathBuf,
        user: Option<&str>,
        pass: Option<&str>,
        plugins: &[&str],
    ) -> SocketAddr {
        let names: Vec<String> = plugins.iter().map(|s| s.to_string()).collect();
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 0,
            dir,
            user: user.map(String::from),
            pass: pass.map(String::from),
            no_open: true,
            verbose: false,
            plugins: plugin::Set::resolve(&names).unwrap(),
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
        http_get_accept(addr, path, ua, auth, None)
    }

    /// The `Location:` value of a raw response, lowercased header name aside.
    fn location(resp: &str) -> String {
        resp.lines()
            .find(|l| l.to_ascii_lowercase().starts_with("location:"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    }

    fn http_get_accept(
        addr: SocketAddr,
        path: &str,
        ua: &str,
        auth: Option<&str>,
        accept: Option<&str>,
    ) -> (u16, String, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: {ua}\r\n");
        if let Some(a) = auth {
            req.push_str(&format!("Authorization: Basic {a}\r\n"));
        }
        if let Some(a) = accept {
            req.push_str(&format!("Accept: {a}\r\n"));
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
    fn listing_and_direct_views() {
        let addr = start_server(setup(), None, None);
        let (status, body, ct) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("a.md"));
        assert!(body.contains("docs/b.md"));
        assert!(body.contains("ignore.txt"));

        let (status, body, _) = http_get(addr, "/a.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("<h1>Alpha</h1>"));

        let (status, body, ct) =
            http_get_accept(addr, "/a.md", "test-agent", None, Some("text/markdown"));
        assert_eq!(status, 200);
        assert!(ct.contains("text/markdown"));
        assert!(body.contains("# Alpha"));

        let (status, _, _) = http_get(addr, "/missing.md", "test-agent", None);
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

        let (status, body, _) = http_get(addr, "/a.md", "Wget/1.21.4", None);
        assert_eq!(status, 200);
        assert!(body.contains("ALPHA"));
        assert!(body.contains("Hello **world**."));
    }

    #[test]
    fn traversal_rejected() {
        let addr = start_server(setup(), None, None);
        for path in [
            "/..%2F..%2Fetc%2Fpasswd",
            "/../a.md",
            "/../../etc/passwd",
            "/a.md%00",
            "/a.md\\..\\x",
        ] {
            let (status, _, _) = http_get(addr, path, "curl/8.7.1", None);
            assert_eq!(status, 404, "path: {path}");
        }
    }

    #[test]
    fn serves_html_files_and_negotiates() {
        let addr = start_server(setup(), None, None);
        let (status, body, _) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("page.html"));

        let (status, body, ct) = http_get(addr, "/page.html", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("<h1>Gamma</h1>"));

        let (status, body, ct) =
            http_get_accept(addr, "/page.html", "test-agent", None, Some("text/markdown"));
        assert_eq!(status, 200);
        assert!(ct.contains("text/markdown"));
        assert!(body.contains("# Gamma"));
        assert!(body.contains("**html**"));

        let (status, body, ct) =
            http_get_accept(addr, "/page.html", "test-agent", None, Some("text/plain"));
        assert_eq!(status, 200);
        assert!(ct.contains("text/plain"));
        assert!(body.contains("Gamma"));
        assert!(!body.contains('<'));
    }

    #[test]
    fn serves_static_assets_with_guessed_mime() {
        let dir = setup();
        fs::write(dir.join("style.css"), "body { color: red; }").unwrap();
        fs::write(dir.join("favicon.ico"), b"\x00\x00\x01\x00").unwrap();
        let addr = start_server(dir, None, None);

        let (status, body, ct) = http_get(addr, "/style.css", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/css"));
        assert!(body.contains("color: red"));

        let (status, _, ct) = http_get(addr, "/favicon.ico", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("image/x-icon"));
    }

    #[test]
    fn index_fallback_for_directories() {
        let dir = setup();
        fs::write(dir.join("docs").join("index.md"), "# Docs Index\n").unwrap();
        let addr = start_server(dir, None, None);

        let (status, body, _) = http_get(addr, "/docs", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("Docs Index"));

        // The spellings that name the same page all point back at /docs.
        for path in ["/docs/", "/docs/index.md", "//docs", "/docs/index.md/"] {
            let (status, resp, _) = http_get(addr, path, "test-agent", None);
            assert_eq!(status, 301, "path: {path}");
            assert_eq!(location(&resp), "/docs", "path: {path}");
        }
    }

    #[test]
    fn repeated_slashes_collapse() {
        let addr = start_server(setup(), None, None);
        for path in ["//a.md", "///a.md", "/////a.md"] {
            let (status, resp, _) = http_get(addr, path, "test-agent", None);
            assert_eq!(status, 301, "path: {path}");
            assert_eq!(location(&resp), "/a.md", "path: {path}");
        }
        let (status, resp, _) = http_get(addr, "////docs///b.md//", "test-agent", None);
        assert_eq!(status, 301);
        assert_eq!(location(&resp), "/docs/b.md");
    }

    #[test]
    fn root_is_left_alone() {
        let addr = start_server(setup(), None, None);
        for path in ["/", "//", "////"] {
            let (status, resp, _) = http_get(addr, path, "test-agent", None);
            if path == "/" {
                assert_eq!(status, 200, "path: {path}");
            } else {
                assert_eq!(status, 301, "path: {path}");
                assert_eq!(location(&resp), "/", "path: {path}");
            }
        }
    }

    #[test]
    fn root_index_is_suppressed() {
        let dir = setup();
        fs::write(dir.join("index.md"), "# Home\n").unwrap();
        let addr = start_server(dir, None, None);

        let (status, resp, _) = http_get(addr, "/index.md", "test-agent", None);
        assert_eq!(status, 301);
        assert_eq!(location(&resp), "/");

        let (status, body, _) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("Home"));
    }

    #[test]
    fn redirects_keep_the_query_string() {
        let dir = setup();
        fs::write(dir.join("docs").join("index.md"), "# Docs Index\n").unwrap();
        let addr = start_server(dir, None, None);

        let (status, resp, _) = http_get(addr, "/docs/index.md?q=1&x=2", "test-agent", None);
        assert_eq!(status, 301);
        assert_eq!(location(&resp), "/docs?q=1&x=2");
    }

    #[test]
    fn redirect_target_is_re_encoded() {
        let dir = tmp_dir("spaces");
        fs::create_dir_all(dir.join("my docs")).unwrap();
        fs::write(dir.join("my docs").join("index.md"), "# Spaced\n").unwrap();
        let addr = start_server(dir, None, None);

        let (status, resp, _) = http_get(addr, "/my%20docs/index.md", "test-agent", None);
        assert_eq!(status, 301);
        assert_eq!(location(&resp), "/my%20docs");
    }

    #[test]
    fn a_shadowed_index_keeps_its_explicit_url() {
        let dir = tmp_dir("shadowed");
        fs::write(dir.join("index.html"), "<h1>Home</h1>").unwrap();
        fs::write(dir.join("index.md"), "# Sidelined\n").unwrap();
        let addr = start_server(dir, None, None);

        // / serves index.html, so /index.md is not a redundant spelling of it.
        let (status, body, _) = http_get(addr, "/index.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("Sidelined"), "{body}");

        let (status, resp, _) = http_get(addr, "/index.html", "test-agent", None);
        assert_eq!(status, 301);
        assert_eq!(location(&resp), "/");
    }

    #[test]
    fn head_redirects_carry_no_body() {
        let addr = start_server(setup(), None, None);
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"HEAD //a.md HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        assert!(resp.starts_with("HTTP/1.1 301"), "{resp}");
        assert_eq!(location(&resp), "/a.md");
        assert!(resp.ends_with("\r\n\r\n"), "{resp}");
    }

    #[test]
    fn root_index_html_takes_precedence_over_listing() {
        let dir = setup();
        fs::write(dir.join("index.html"), "<h1>Home</h1>").unwrap();
        let addr = start_server(dir, None, None);

        let (status, body, ct) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("<h1>Home</h1>"));
        assert!(!body.contains("Serving"));
    }

    #[test]
    fn directory_without_index_falls_back_to_listing() {
        let addr = start_server(setup(), None, None);
        let (status, body, ct) = http_get(addr, "/docs", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("docs/b.md"));
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

    fn math_dir() -> PathBuf {
        let dir = tmp_dir("math");
        fs::write(
            dir.join("math.md"),
            concat!(
                "# Euler\n\nThe identity $e^{i\\pi} + 1 = 0$ links five constants.\n\n",
                "$$\\int_0^\\infty e^{-x^2} dx$$\n"
            ),
        )
        .unwrap();
        fs::write(dir.join("plain.md"), "# Plain\n\nNo formulas here.\n").unwrap();
        dir
    }

    #[test]
    fn math_plugin_renders_mathml() {
        let addr = start_server_with(math_dir(), None, None, &["math"]);
        let (status, body, ct) = http_get(addr, "/math.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/html"));
        assert!(body.contains("<math"), "{body}");
        assert!(body.contains("display=\"block\""), "{body}");
        // Fully server-rendered: no client-side typesetting is shipped.
        assert!(!body.contains("<script"), "{body}");
        // The plugin's style block rides along only on pages it touched.
        assert!(body.contains("<style"), "{body}");
        let (_, plain, _) = http_get(addr, "/plain.md", "test-agent", None);
        assert!(!plain.contains("<style"), "{plain}");
    }

    #[test]
    fn math_is_inert_without_the_plugin() {
        let addr = start_server(math_dir(), None, None);
        let (status, body, _) = http_get(addr, "/math.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(!body.contains("<math"), "{body}");
        assert!(!body.contains("<style"), "{body}");
    }

    #[test]
    fn math_reaches_terminals_as_latex() {
        let addr = start_server_with(math_dir(), None, None, &["math"]);
        let (status, body, ct) = http_get(addr, "/math.md", "curl/8.0", None);
        assert_eq!(status, 200);
        assert!(ct.contains("text/plain"));
        assert!(body.contains("$e^{i\\pi} + 1 = 0$"), "{body}");
        assert!(!body.contains("<math"), "{body}");
    }

    #[test]
    fn math_source_survives_the_markdown_route() {
        let addr = start_server_with(math_dir(), None, None, &["math"]);
        let (status, body, ct) =
            http_get_accept(addr, "/math.md", "test-agent", None, Some("text/markdown"));
        assert_eq!(status, 200);
        assert!(ct.contains("text/markdown"));
        assert!(body.contains("$e^{i\\pi} + 1 = 0$"), "{body}");
    }

    #[test]
    fn mermaid_plugin_renders_inline_svg() {
        let dir = tmp_dir("mermaid");
        fs::write(
            dir.join("flow.md"),
            "# Flow\n\n```mermaid\nflowchart LR\n  A[Start] --> B[End]\n```\n",
        )
        .unwrap();
        let addr = start_server_with(dir.clone(), None, None, &["mermaid"]);
        let (status, body, _) = http_get(addr, "/flow.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("<svg"), "{body}");
        assert!(body.contains(">Start<"), "{body}");
        // Server-rendered: nothing executable reaches the client.
        assert!(!body.contains("<script"), "{body}");

        // Off by default, the fence stays an ordinary code block.
        let plain = start_server(dir, None, None);
        let (_, body, _) = http_get(plain, "/flow.md", "test-agent", None);
        assert!(!body.contains("<svg"), "{body}");
        assert!(body.contains("language-mermaid"), "{body}");
    }

    #[test]
    fn both_plugins_can_run_together() {
        let dir = tmp_dir("both");
        fs::write(
            dir.join("mixed.md"),
            "$E = mc^2$\n\n```mermaid\nflowchart TD\n  A --> B\n```\n",
        )
        .unwrap();
        let addr = start_server_with(dir, None, None, &["math", "mermaid"]);
        let (status, body, _) = http_get(addr, "/mixed.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("<math"), "{body}");
        assert!(body.contains("<svg"), "{body}");
        // Each plugin contributes its own <head> block.
        assert_eq!(body.matches("<style>").count(), 2, "{body}");
    }
}
