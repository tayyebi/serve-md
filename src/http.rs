use crate::auth::Auth;
use crate::cli::Config;
use crate::encoding::{percent_decode, percent_encode_path};
use crate::mime;
use crate::page;
use crate::plugin;
use crate::render;
use crate::scanner::{is_forbidden_segment, scan, FileEntry, FileKind};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Ceilings on what a single client can spend of the server's resources.
/// Each connection owns a thread, so without a cap on how many are live,
/// how long one may dawdle over its request, and how long it may take to
/// read the answer, opening sockets is enough to wedge the process.
const MAX_HEADER: usize = 65_536;
const MAX_HEADERS: usize = 128;
const MAX_LIVE_CONNECTIONS: usize = 128;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How much of one attacker-supplied field reaches a `--verbose` log line.
const MAX_LOG_FIELD: usize = 512;

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
    /// Connections currently being served, against `MAX_LIVE_CONNECTIONS`.
    live: AtomicUsize,
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
        live: AtomicUsize::new(0),
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
        if ctx.live.fetch_add(1, Ordering::AcqRel) >= MAX_LIVE_CONNECTIONS {
            ctx.live.fetch_sub(1, Ordering::AcqRel);
            let _ = refuse_busy(&mut s);
            continue;
        }
        let c = Arc::clone(&ctx);
        thread::spawn(move || {
            let _ = handle_connection(&mut s, &c);
            c.live.fetch_sub(1, Ordering::AcqRel);
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
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
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
    // Origin-form only. A target that is not a rooted path — absolute-form
    // `http://host/x`, authority-form, `*` — is not something this server
    // routes, and letting one fall through to path resolution only widens
    // what the checks below have to hold for.
    if !decoded.starts_with('/') {
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
    let canonical = canonical_path(&ctx.dir, &decoded);
    if canonical != decoded {
        return redirect(stream, &canonical, query, is_head);
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
        if stream_file(stream, full, mime::guess(full), is_head)? {
            return Ok(());
        }
        return not_found(stream, terminal, is_head);
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

/// Streams a file from disk to the socket instead of buffering it in memory
/// first: a request for a large asset must not cost the server that asset's
/// size in RAM, and it never has to, since `Content-Length` can come from
/// the same handle the bytes do.
///
/// `Ok(false)` means nothing has been written yet and the caller should
/// answer 404 — once the head goes out it is too late to change the status.
fn stream_file(
    stream: &mut TcpStream,
    full: &Path,
    ctype: &str,
    is_head: bool,
) -> io::Result<bool> {
    let Ok(file) = fs::File::open(full) else {
        return Ok(false);
    };
    let Ok(meta) = file.metadata() else {
        return Ok(false);
    };
    if !meta.is_file() {
        return Ok(false);
    }
    let len = meta.len();
    write_head(stream, 200, "OK", ctype, len, &[])?;
    if !is_head {
        // Capped at the length just announced: if the file grows underneath
        // us mid-copy, the body still matches its own Content-Length.
        io::copy(&mut file.take(len), stream)?;
    }
    stream.flush()?;
    Ok(true)
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

/// Hard caps on the shape of a request path, so a pathological URL cannot
/// make the server walk an unbounded amount of it before saying no.
const MAX_PATH_LEN: usize = 4096;
const MAX_PATH_DEPTH: usize = 64;

/// Vets one decoded request path and builds the path it names under `root`.
///
/// Everything here is a filter on the request *string*, applied before the
/// filesystem is touched at all. A segment has to be one plain file name:
/// control bytes (NUL included), backslashes, `.`, `..`, hidden and skipped
/// names, and anything that is not a `Normal` component — a Windows drive
/// letter, a UNC or verbatim prefix, an absolute segment, each of which
/// would otherwise *replace* the root outright when joined — are all
/// refused. The canonicalize-and-contain check in `resolve` is the second
/// line of defence behind this, not the first.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let trimmed = rel.trim_end_matches('/');
    if trimmed.len() > MAX_PATH_LEN || trimmed.contains('\\') {
        return None;
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return None;
    }
    let mut out = root.to_path_buf();
    if trimmed.is_empty() {
        return Some(out);
    }
    if trimmed.split('/').count() > MAX_PATH_DEPTH {
        return None;
    }
    for seg in trimmed.split('/') {
        if seg.is_empty() || is_forbidden_segment(seg) {
            return None;
        }
        let mut comps = Path::new(seg).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(name)), None) if name.to_str() == Some(seg) => out.push(name),
            _ => return None,
        }
    }
    Some(out)
}

/// Whether a path that already resolved inside `root` still lands on a name
/// the server refuses to serve.
///
/// `safe_join` vets what the *request* spelled; this vets where the request
/// actually landed. They differ whenever a symlink is involved: `docs/link`
/// pointing at `../.git` spells nothing forbidden, but `/docs/link/config`
/// would hand out the repository all the same.
fn lands_on_forbidden(root: &Path, full: &Path) -> bool {
    let Ok(rel) = full.strip_prefix(root) else {
        return true;
    };
    rel.components().any(|c| match c {
        Component::Normal(name) => is_forbidden_segment(&name.to_string_lossy()),
        Component::CurDir => false,
        _ => true,
    })
}

/// Resolves a percent-decoded, leading-slash-stripped request path to a file
/// under `root`. A path that names a directory (including the served root
/// itself) falls back to `index.html`, then `index.md`, inside it; if
/// neither exists, the caller should show the full-tree listing page.
///
/// Every exit is the same `NotFound` the caller renders as a plain 404:
/// refusing to serve a path and not having it look identical from outside,
/// so the router never confirms that a file it will not hand over exists.
fn resolve(root: &Path, rel: &str) -> Resolved {
    let Ok(root_c) = root.canonicalize() else {
        return Resolved::NotFound;
    };
    let Some(candidate) = safe_join(&root_c, rel) else {
        return Resolved::NotFound;
    };
    let Ok(cand_c) = candidate.canonicalize() else {
        return Resolved::NotFound;
    };
    if !cand_c.starts_with(&root_c) || lands_on_forbidden(&root_c, &cand_c) {
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
                idx_c.starts_with(&root_c)
                    && !lands_on_forbidden(&root_c, idx_c)
                    && fs::metadata(idx_c).map(|m| m.is_file()).unwrap_or(false)
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
    let start = Instant::now();
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
        // The socket's read timeout does not bound this on its own: a client
        // dribbling one byte at a time restarts it on every read and would
        // otherwise hold the thread indefinitely.
        if start.elapsed() > HEAD_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "request headers too slow",
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
            // Dropping the excess instead would fail open in the wrong
            // direction — a request could bury its Authorization header past
            // the cap — so an over-long header list ends the connection.
            if headers.len() >= MAX_HEADERS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "too many request headers",
                ));
            }
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
    line.push_str(&format!(
        "{} {} ",
        log_safe(&req.method),
        log_safe(&req.target)
    ));
    let wrapped = wrap_text(&line, 80);
    println!("[verbose] request: {}", wrapped);
    for (k, v) in &req.headers {
        println!("[verbose]   {}: {}", log_safe(k), log_safe(v));
    }
    println!("[verbose]   user-agent: {}", log_safe(&req.ua));
    println!("[verbose]   terminal: {}", terminal);
    println!("[verbose]   auth: {}", ctx.auth.is_some());
}

/// Renders one field of a request for the operator's terminal. The text is
/// entirely attacker-controlled, so control bytes are replaced rather than
/// printed: otherwise a crafted header could move the cursor, repaint the
/// screen, or forge whole log lines with ANSI escapes and newlines.
fn log_safe(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(MAX_LOG_FIELD)
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect();
    if s.chars().nth(MAX_LOG_FIELD).is_some() {
        out.push('…');
    }
    out
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
    len: u64,
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

/// Turns a connection away when the server is already at its ceiling. Best
/// effort, on a short timeout: this runs on the accept loop, so a client
/// that never reads its 503 must not be able to stall the ones behind it.
fn refuse_busy(stream: &mut TcpStream) -> io::Result<()> {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    respond(
        stream,
        503,
        "Service Unavailable",
        "text/plain; charset=utf-8",
        b"503 Service Unavailable\n",
        &[("Retry-After", "1")],
        false,
    )
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
    write_head(stream, status, reason, ctype, body.len() as u64, extra)?;
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
    fn hidden_and_vcs_paths_are_never_served() {
        let dir = setup();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git").join("config"), "[core]\n").unwrap();
        fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::write(dir.join("node_modules").join("pkg.md"), "# pkg\n").unwrap();
        fs::write(dir.join("docs").join(".secret.md"), "# secret\n").unwrap();
        let addr = start_server(dir, None, None);

        for path in [
            "/.git/config",
            "/.env",
            "/node_modules/pkg.md",
            "/docs/.secret.md",
        ] {
            let (status, _, _) = http_get(addr, path, "test-agent", None);
            assert_eq!(status, 404, "path: {path}");
        }

        // The listing hid these all along; now the router agrees.
        let (_, body, _) = http_get(addr, "/", "test-agent", None);
        assert!(!body.contains(".env"), "{body}");
        assert!(!body.contains("pkg.md"), "{body}");
        assert!(!body.contains(".secret.md"), "{body}");
    }

    #[test]
    fn well_known_stays_reachable() {
        let dir = setup();
        fs::create_dir_all(dir.join(".well-known")).unwrap();
        fs::write(
            dir.join(".well-known").join("security.txt"),
            "Contact: mailto:x@example.com\n",
        )
        .unwrap();
        let addr = start_server(dir, None, None);

        let (status, body, _) = http_get(addr, "/.well-known/security.txt", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("Contact:"), "{body}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_smuggle_a_path_out_or_back_in() {
        use std::os::unix::fs::symlink;
        let dir = setup();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git").join("config"), "[core]\n").unwrap();
        // Spells nothing forbidden, lands somewhere forbidden.
        symlink(dir.join(".git"), dir.join("link")).unwrap();
        // Lands outside the served tree entirely.
        symlink("/etc/passwd", dir.join("passwd.md")).unwrap();
        let addr = start_server(dir, None, None);

        for path in ["/link/config", "/link", "/passwd.md"] {
            let (status, _, _) = http_get(addr, path, "test-agent", None);
            assert_eq!(status, 404, "path: {path}");
        }
    }

    #[test]
    fn pathological_paths_are_rejected() {
        let addr = start_server(setup(), None, None);
        let deep = format!("/{}", ["a"; 100].join("/"));
        let long = format!("/{}.md", "a".repeat(5000));
        for path in [
            deep.as_str(),
            long.as_str(),
            "/a%01.md",  // control byte
            "/a%0A.md",  // newline
            "/a%00.md",  // NUL
        ] {
            let (status, _, _) = http_get(addr, path, "test-agent", None);
            assert_eq!(status, 404, "path: {path}");
        }
    }

    #[test]
    fn non_origin_form_targets_are_refused() {
        let addr = start_server(setup(), None, None);
        for target in ["http://example.com/a.md", "a.md", "*"] {
            let (status, _, _) = http_get(addr, target, "test-agent", None);
            assert_eq!(status, 400, "target: {target}");
        }
    }

    #[test]
    fn static_files_stream_with_an_exact_length() {
        let dir = setup();
        let big = vec![b'x'; 300 * 1024];
        fs::write(dir.join("big.bin"), &big).unwrap();
        let addr = start_server(dir, None, None);

        let (status, resp, ct) = http_get(addr, "/big.bin", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("application/octet-stream"));
        assert!(
            resp.contains(&format!("Content-Length: {}", big.len())),
            "no matching Content-Length"
        );
        assert!(resp.ends_with(&"x".repeat(64)));

        // HEAD announces the same length without reading the file out.
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"HEAD /big.bin HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains(&format!("Content-Length: {}", big.len())), "{resp}");
        assert!(resp.ends_with("\r\n\r\n"), "{resp}");
    }

    #[test]
    fn too_many_headers_ends_the_connection() {
        let addr = start_server(setup(), None, None);
        let mut s = TcpStream::connect(addr).unwrap();
        let mut req = String::from("GET /a.md HTTP/1.1\r\nHost: localhost\r\n");
        for i in 0..(MAX_HEADERS + 10) {
            req.push_str(&format!("X-Pad-{i}: v\r\n"));
        }
        req.push_str("\r\n");
        let _ = s.write_all(req.as_bytes());
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        assert!(resp.is_empty(), "{resp}");
    }

    #[test]
    fn safe_join_refuses_anything_but_plain_names() {
        let root = tmp_dir("join");
        for bad in [
            "..",
            "a/../b",
            ".",
            "./a",
            ".git/config",
            ".env",
            "a\\b",
            "a\0b",
            "a//b",
        ] {
            assert!(safe_join(&root, bad).is_none(), "accepted: {bad:?}");
        }
        assert!(safe_join(&root, &"a/".repeat(MAX_PATH_DEPTH + 1)).is_none());
        assert!(safe_join(&root, &"a".repeat(MAX_PATH_LEN + 1)).is_none());

        assert_eq!(safe_join(&root, "").unwrap(), root);
        assert_eq!(safe_join(&root, "a/b.md").unwrap(), root.join("a/b.md"));
        assert_eq!(
            safe_join(&root, ".well-known/x").unwrap(),
            root.join(".well-known/x")
        );
    }

    #[test]
    fn log_fields_lose_their_control_bytes() {
        let out = log_safe("GET /a\u{1b}[2Jb\nc");
        assert!(!out.contains('\u{1b}'), "{out}");
        assert!(!out.contains('\n'), "{out}");
        assert!(out.contains("[2Jb"), "{out}");
        assert!(!log_safe("short").ends_with('…'));
        assert!(log_safe(&"a".repeat(MAX_LOG_FIELD + 1)).ends_with('…'));
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
