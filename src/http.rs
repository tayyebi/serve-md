use crate::auth::Auth;
use crate::catalog::Catalog;
use crate::cli::Config;
use crate::encoding::{percent_decode, percent_encode_path};
use crate::json::Value;
use crate::llms;
use crate::mcp;
use crate::mime;
use crate::page;
use crate::plugin;
use crate::render;
use crate::scanner::{is_forbidden_segment, FileEntry, FileKind};
use crate::search;
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
/// The largest request body accepted. Only the MCP endpoint reads one, and a
/// JSON-RPC call naming a document and a query has no business approaching
/// this.
const MAX_BODY: usize = 1024 * 1024;

/// The path the MCP endpoint answers on.
///
/// A single path serving POST, which is all revision 2026-07-28 requires: the
/// GET stream endpoint and the DELETE session teardown were both removed.
const MCP_PATH: &str = "/mcp";
/// Generated when the served tree does not contain a file of the same name.
const LLMS_PATH: &str = "/llms.txt";
const LLMS_FULL_PATH: &str = "/llms-full.txt";
/// The server card, so a client can discover the endpoint without guessing.
/// `.well-known` is already the one hidden directory the router will serve.
const MCP_CARD_PATH: &str = "/.well-known/mcp.json";

/// Cross-origin headers for the MCP endpoint.
///
/// serve-md is meant to be deployed publicly, where the callers are hosted
/// agents. Those reach the endpoint server-to-server and send no `Origin` at
/// all, so there is no origin to check; browser-resident agents do send one,
/// and need these headers to be allowed to read the reply. The endpoint
/// exposes exactly the documents the website already serves — and sits behind
/// the same `--user`/`--pass` when set — so opening it adds no reach that a
/// plain GET did not already have.
const CORS: &[(&str, &str)] = &[
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "POST, OPTIONS"),
    (
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization, MCP-Protocol-Version, Mcp-Method, Mcp-Name",
    ),
    ("Access-Control-Max-Age", "86400"),
];

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    ua: String,
    accept: String,
    /// Empty for every method but POST. Read only up to [`MAX_BODY`].
    body: String,
}

struct Ctx {
    dir: PathBuf,
    /// The served file list. Shared with the MCP tools and `llms.txt`, and
    /// refreshed by a watcher under `--fresh`.
    catalog: Arc<Catalog>,
    auth: Option<Auth>,
    verbose: bool,
    plugins: plugin::Set,
    /// Whether `--plugin webmcp` was given, which gates `/mcp`, `/llms.txt`
    /// and the server card.
    mcp: bool,
    /// The search tool found at startup, if any.
    engine: Option<search::Engine>,
    /// Connections currently being served, against `MAX_LIVE_CONNECTIONS`.
    live: AtomicUsize,
}

impl Ctx {
    /// The `<head>` markup the active plugins contribute to every page.
    ///
    /// `math` and `mermaid` add markup only to documents they changed, so they
    /// contribute nothing here; `webmcp` registers its tools on every page,
    /// including the listing, which has no Markdown to transform at all.
    fn page_head(&self) -> String {
        self.plugins.render_html("").head
    }
}

pub fn serve(cfg: Config) -> io::Result<()> {
    let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))?;
    serve_on(cfg, listener)
}

fn serve_on(cfg: Config, listener: TcpListener) -> io::Result<()> {
    let catalog = Arc::new(Catalog::scan(&cfg.dir)?);
    if cfg.fresh {
        catalog.spawn_watcher(cfg.fresh_interval, cfg.verbose);
    }
    let auth = match (cfg.user.as_ref(), cfg.pass.as_ref()) {
        (Some(u), Some(p)) => Some(Auth::new(u.clone(), p.clone())),
        _ => None,
    };
    let mcp = cfg.plugins.has("webmcp");
    // Probed once here rather than per query, so a missing search tool is
    // reported in the banner instead of at the first `search_docs` call.
    let engine = if mcp { search::detect() } else { None };
    let ctx = Arc::new(Ctx {
        dir: cfg.dir.clone(),
        catalog,
        auth,
        verbose: cfg.verbose,
        plugins: cfg.plugins,
        mcp,
        engine,
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
    if cfg.fresh {
        println!("watching: every {}ms", cfg.fresh_interval.as_millis());
    }
    println!("  {url}");
    if ctx.mcp {
        println!("  {url}mcp          Model Context Protocol");
        println!("  {url}llms.txt     index for language models");
        match ctx.engine {
            Some(e) => println!("search: {}", e.binary()),
            // Worth a line of its own: everything else works, and the failure
            // would otherwise only surface inside a tool call an agent made.
            None => println!("search: DISABLED — no rg, ag or grep on PATH"),
        }
    }
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
    let terminal = ua_is_terminal(&req.ua);
    verbose_log(&req, terminal, ctx);

    let on_mcp = ctx.mcp && path_of(&req.target) == MCP_PATH;

    // Answered before the auth check on purpose. A CORS preflight carries no
    // credentials — browsers do not send them — so requiring auth here would
    // block every cross-origin agent before it ever got the chance to
    // authenticate on the real request.
    if req.method == "OPTIONS" {
        if on_mcp {
            return respond(stream, 204, "No Content", "text/plain", b"", CORS, false);
        }
        return method_not_allowed(stream, "GET, HEAD", is_head);
    }

    if req.method == "POST" {
        if !on_mcp {
            return method_not_allowed(stream, "GET, HEAD", is_head);
        }
    } else if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(stream, "GET, HEAD", is_head);
    }
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

    if req.method == "POST" {
        return serve_mcp(ctx, stream, &req);
    }
    route(ctx, stream, &req, terminal, is_head)
}

fn method_not_allowed(stream: &mut TcpStream, allow: &str, is_head: bool) -> io::Result<()> {
    respond(
        stream,
        405,
        "Method Not Allowed",
        "text/plain; charset=utf-8",
        b"405 Method Not Allowed\n",
        &[("Allow", allow)],
        is_head,
    )
}

/// The path part of a request target, without the query.
fn path_of(target: &str) -> &str {
    target.split('?').next().unwrap_or(target)
}

/// Answers one MCP request.
///
/// The body has already been read and bounded by `read_request`; everything
/// protocol-shaped happens in [`mcp::handle`], which is transport-agnostic and
/// tested on its own.
fn serve_mcp(ctx: &Ctx, stream: &mut TcpStream, req: &Request) -> io::Result<()> {
    let snap = ctx.catalog.current();
    let mcp_ctx = mcp::Ctx {
        root: &ctx.dir,
        snap: &snap,
        plugins: &ctx.plugins,
        engine: ctx.engine,
    };
    let reply = mcp::handle(&req.body, &req.headers, &mcp_ctx);
    respond(
        stream,
        reply.status,
        reply.reason,
        "application/json",
        reply.body.as_bytes(),
        CORS,
        false,
    )
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
    // Resolved before canonicalisation, because the path-to-file mapping below
    // is total: it has no seam for a path that names no file on disk. Every
    // one of these yields to a real file of the same name, so an author who
    // writes their own `llms.txt` is served theirs.
    if ctx.mcp {
        if let Some(result) = synthetic(ctx, stream, &decoded, is_head) {
            return result;
        }
    }

    let canonical = canonical_path(&ctx.dir, &decoded);
    if canonical != decoded {
        return redirect(stream, &canonical, query, is_head);
    }
    let rel = decoded.strip_prefix('/').unwrap_or(&decoded);

    match resolve(&ctx.dir, rel) {
        Resolved::File(full, kind) => serve_file(ctx, stream, &full, kind, terminal, &req.accept, is_head),
        Resolved::Listing(under) => listing(ctx, stream, &under, terminal, is_head),
        Resolved::NotFound => match literal_escape_target(&ctx.dir, target_path, &decoded) {
            Some(canonical) => redirect(stream, &canonical, query, is_head),
            None => not_found(stream, terminal, is_head),
        },
    }
}

/// The canonical path for a request whose escapes should have been literal
/// text, or `None` when there is no such file.
///
/// A file can be named `%d8%a2.md` — WordPress exports do exactly this, saving
/// a percent-encoded slug as a filename. Its correct URL double-encodes the
/// percent signs, but every link written to it says `%d8%a2.md`, which decodes
/// to a name that is not on disk. Rather than serve one document under two
/// spellings, the wrong one redirects to the right one, the same answer this
/// server already gives a trailing slash or a redundant `index.md`.
///
/// The returned path is *undecoded* on purpose: `redirect` percent-encodes it,
/// which turns each `%` into `%25` and yields the double-encoded canonical
/// form. Requesting that decodes back to the name on disk, so the redirect
/// resolves in one hop and cannot loop.
fn literal_escape_target(root: &Path, raw: &str, decoded: &str) -> Option<String> {
    // No escapes means decoding changed nothing and this cannot apply.
    if raw == decoded || !raw.starts_with('/') {
        return None;
    }
    let rel = raw.strip_prefix('/').unwrap_or(raw);
    // Vetted by the same `resolve` the website uses, so a literal-escape path
    // gets no reach a normal one would not.
    match resolve(root, rel) {
        Resolved::NotFound => None,
        _ => Some(raw.to_string()),
    }
}

/// The file listing for one directory. `under` is its root-relative path, or
/// empty for the served root.
///
/// The catalog is a flat snapshot of the whole tree, so the directory a
/// request actually named is applied here as a filter. Serving the entire
/// tree for `/a/b` would answer a question nobody asked and, in a deep tree,
/// bury the handful of files that directory holds.
fn listing(
    ctx: &Ctx,
    stream: &mut TcpStream,
    under: &str,
    terminal: bool,
    is_head: bool,
) -> io::Result<()> {
    let snap = ctx.catalog.current();
    let files = files_under(&snap.files, under);
    let dir = if under.is_empty() {
        ctx.dir.clone()
    } else {
        ctx.dir.join(under)
    };
    if terminal {
        let body = page::listing_plain(&files, &dir, ctx.mcp);
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
        // The listing has no Markdown to transform, so without passing the
        // head markup explicitly the `webmcp` script would reach every
        // document page and not the page most visitors land on first.
        let body = page::listing_html(&files, &ctx.page_head());
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

/// The catalog entries a listing for `under` should show.
///
/// The root listing stays the whole tree: it is the site's index, and what
/// `curl <base>/` is documented to return. A subdirectory listing is scoped to
/// that directory and one level deep, so it shows what a file manager would —
/// its own files, with nested directories left to their own listings.
///
/// Entries are cloned because the snapshot is shared behind an `Arc` and the
/// page renderers take a slice; a subdirectory's worth is a small copy.
fn files_under(files: &[FileEntry], under: &str) -> Vec<FileEntry> {
    if under.is_empty() {
        return files.to_vec();
    }
    files
        .iter()
        .filter(|f| {
            f.rel
                .strip_prefix(under)
                .and_then(|tail| tail.strip_prefix('/'))
                .is_some_and(|tail| !tail.contains('/'))
        })
        .cloned()
        .collect()
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
        FileKind::Markdown => page::view_html(&rel, &rendered.html, &rendered.head),
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

/// Serves the paths that are generated rather than read from disk, or `None`
/// if this request is not one of them and should fall through to the ordinary
/// file mapping.
fn synthetic(
    ctx: &Ctx,
    stream: &mut TcpStream,
    path: &str,
    is_head: bool,
) -> Option<io::Result<()>> {
    let snap = ctx.catalog.current();
    // A file the author wrote always wins over a file this server would
    // invent. Checked first, and for every path, so the rule needs no
    // per-route repetition.
    if snap.contains(path.trim_start_matches('/')) {
        return None;
    }

    // The body is built first and written once at the end, rather than through
    // a closure per arm: a closure capturing `stream` would hold a mutable
    // borrow across the whole match, which the `MCP_PATH` arm also needs.
    let (body, ctype) = match path {
        // 2026-07-28 §"Backward Compatibility": a server implementing only
        // this revision answers GET or DELETE on the MCP endpoint with 405,
        // since the GET stream and the DELETE session teardown are gone.
        MCP_PATH => return Some(method_not_allowed(stream, "POST, OPTIONS", is_head)),
        LLMS_PATH => (
            llms::llms_txt(&ctx.dir, &snap, &ctx.plugins.options(), ctx.mcp),
            "text/plain; charset=utf-8",
        ),
        LLMS_FULL_PATH => (
            llms::llms_full_txt(&ctx.dir, &snap, &ctx.plugins.options()),
            "text/plain; charset=utf-8",
        ),
        MCP_CARD_PATH => (server_card(), "application/json"),
        _ => return None,
    };
    Some(respond(stream, 200, "OK", ctype, body.as_bytes(), &[], is_head))
}

/// The MCP server card: what this endpoint is, before a client connects to it.
///
/// Follows the shape proposed in SEP-1649, "MCP Server Cards — HTTP Server
/// Discovery via .well-known" — an `mcp` object holding `spec_version`, a
/// `servers` array and a `tools` array.
/// <https://github.com/modelcontextprotocol/modelcontextprotocol/issues/1649>
///
/// That SEP is a proposal, not a ratified part of the specification, so this
/// file is a convenience for clients that look for it and is ignored by
/// everything else. The endpoint works without it.
///
/// The URL is relative on purpose: the server does not reliably know the host
/// and scheme it is reached through, and inventing one from the `Host` header
/// would be a guess a proxy could make wrong.
fn server_card() -> String {
    let tools: Vec<Value> = mcp::tools::definitions()
        .iter()
        .map(|t| {
            Value::obj([
                ("name", t.get("name").cloned().unwrap_or(Value::Null)),
                (
                    "description",
                    t.get("description").cloned().unwrap_or(Value::Null),
                ),
            ])
        })
        .collect();

    let card = Value::obj([(
        "mcp",
        Value::obj([
            ("spec_version", Value::str("2025-11-25")),
            (
                "servers",
                Value::Arr(vec![Value::obj([
                    ("name", Value::str("serve-md")),
                    ("version", Value::str(env!("CARGO_PKG_VERSION"))),
                    (
                        "description",
                        Value::str("Markdown and HTML documents served from a directory."),
                    ),
                    ("url", Value::str(MCP_PATH)),
                    ("transport", Value::str("streamable-http")),
                    (
                        "protocol_versions",
                        Value::Arr(mcp::SUPPORTED.iter().map(|v| Value::str(*v)).collect()),
                    ),
                ])]),
            ),
            ("tools", Value::Arr(tools)),
        ]),
    )]);
    crate::json::write(&card)
}

enum Resolved {
    File(PathBuf, FileKind),
    /// A directory with no index file, named by its root-relative path —
    /// empty for the served root itself. The path is what scopes the listing
    /// to that directory instead of showing the whole tree.
    Listing(String),
    NotFound,
}

/// Resolves a served document for a caller outside this module.
///
/// The MCP tools need to turn an agent-supplied path into a file, and must do
/// it through exactly the checks the website uses — `safe_join`'s filter on the
/// request string, then the canonicalise-and-contain test, then the
/// forbidden-segment test on where it actually landed. Exposing this is what
/// keeps there from being a second, subtly different implementation of the
/// server's path security.
pub(crate) fn resolve_document(root: &Path, rel: &str) -> Option<(PathBuf, FileKind)> {
    match resolve(root, rel) {
        Resolved::File(full, kind) => Some((full, kind)),
        Resolved::Listing(_) | Resolved::NotFound => None,
    }
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
        return Resolved::Listing(rel.trim_matches('/').to_string());
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
    // The separator's own length is needed to find where the body starts, so
    // the two forms are matched separately rather than folded together.
    let (head_end, sep_len) = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (i, 4),
        None => match buf.windows(2).position(|w| w == b"\n\n") {
            Some(i) => (i, 2),
            None => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed request"))
            }
        },
    };
    // Scoped so the borrow of `buf` ends before the body is split off it.
    let (method, target, headers) = {
        let head = String::from_utf8_lossy(&buf[..head_end]);
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            // Dropping the excess instead would fail open in the wrong
            // direction — a request could bury its Authorization header past
            // the cap — so an over-long header list ends the connection.
            if let Some(ci) = line.find(':') {
                if headers.len() >= MAX_HEADERS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "too many request headers",
                    ));
                }
                headers.push((line[..ci].trim().to_string(), line[ci + 1..].trim().to_string()));
            }
        }
        (method, target, headers)
    };

    // Only POST carries one, and only the MCP endpoint accepts POST. Anything
    // a GET sent as a body is left unread on the socket, which is harmless
    // given every response closes the connection.
    let body = if method == "POST" {
        let leftover = buf.split_off(head_end + sep_len);
        read_body(stream, &headers, leftover, start)?
    } else {
        String::new()
    };

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
        body,
    }))
}

/// Reads a request body of exactly `Content-Length` bytes, under the same
/// deadline as the head.
///
/// Ending the connection rather than answering 413 on an over-long body is
/// deliberate and matches what an over-long header list already does: the
/// request has not been understood, and reading megabytes of it only to
/// discard them is work an attacker chose for the server.
fn read_body(
    stream: &mut TcpStream,
    headers: &[(String, String)],
    mut have: Vec<u8>,
    start: Instant,
) -> io::Result<String> {
    // Refused rather than parsed. A body whose length the server must compute
    // from the body itself is where request smuggling lives, and no MCP client
    // needs chunked encoding to send a few hundred bytes of JSON.
    if headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transfer-encoding is not supported",
        ));
    }

    let len = match headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        Some((_, v)) => v.trim().parse::<usize>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid content-length")
        })?,
        None => 0,
    };
    if len > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body too large",
        ));
    }

    // Whatever arrived alongside the head, capped: bytes past `len` belong to
    // a pipelined request this server does not serve.
    have.truncate(len);
    let mut tmp = [0u8; 4096];
    while have.len() < len {
        if start.elapsed() > HEAD_TIMEOUT {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request body too slow"));
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        let wanted = (len - have.len()).min(n);
        have.extend_from_slice(&tmp[..wanted]);
    }
    Ok(String::from_utf8_lossy(&have).into_owned())
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
    use std::time::SystemTime;

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
            fresh: false,
            fresh_interval: Duration::from_millis(1000),
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

    /// Sends a raw request and returns `(status, whole response)`.
    fn http_send(addr: SocketAddr, req: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
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
        (status, resp)
    }

    fn http_post(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
        auth: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(a) = auth {
            req.push_str(&format!("Authorization: Basic {a}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        http_send(addr, &req)
    }

    /// The body of a raw response.
    fn body_of(resp: &str) -> &str {
        resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
    }

    /// Calls one MCP tool and returns the text content it produced.
    fn call_tool(addr: SocketAddr, name: &str, args: &str) -> String {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
        );
        let (status, resp) = http_post(addr, "/mcp", &[], None, &body);
        assert_eq!(status, 200, "{resp}");
        body_of(&resp).to_string()
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
    fn a_literal_percent_filename_redirects_to_its_canonical_url() {
        // What a WordPress export leaves on disk: the percent-encoded slug
        // saved verbatim as the filename.
        let dir = tmp_dir("literal-escape");
        fs::write(dir.join("%d8%a2.md"), "# Persian slug\n").unwrap();
        let addr = start_server(dir, None, None);

        // The spelling every generated link uses. It decodes to a name that is
        // not on disk, so it is not this document's URL -- but it points
        // unambiguously at it.
        let (status, resp, _) = http_get(addr, "/%d8%a2.md", "test-agent", None);
        assert_eq!(status, 301);
        assert_eq!(location(&resp), "/%25d8%25a2.md");

        // One hop, and the canonical URL serves the file.
        let (status, body, _) = http_get(addr, "/%25d8%25a2.md", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("Persian slug"));
    }

    #[test]
    fn a_missing_file_still_404s_when_its_escapes_are_literal() {
        let dir = tmp_dir("literal-escape-miss");
        fs::write(dir.join("real.md"), "# Real\n").unwrap();
        let addr = start_server(dir, None, None);

        // Nothing on disk under either spelling: no redirect to offer, and no
        // hint that the two readings were tried.
        let (status, _, _) = http_get(addr, "/%d8%a2.md", "test-agent", None);
        assert_eq!(status, 404);
    }

    #[test]
    fn literal_escape_lookup_is_scoped_to_real_escapes() {
        let dir = tmp_dir("literal-escape-unit");
        fs::write(dir.join("%41.md"), "# Encoded A\n").unwrap();
        fs::write(dir.join("A.md"), "# Plain A\n").unwrap();

        // An escape that decodes to a file that exists never reaches this
        // path, but even asked directly it reports the literal reading only
        // when that names something.
        assert_eq!(
            literal_escape_target(&dir, "/%41.md", "/A.md"),
            Some("/%41.md".to_string())
        );
        // A path with no escapes decoded to itself; there is no second
        // reading to try.
        assert_eq!(literal_escape_target(&dir, "/A.md", "/A.md"), None);
        assert_eq!(literal_escape_target(&dir, "/%42.md", "/B.md"), None);
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
    fn a_subdirectory_listing_shows_only_that_directory() {
        let dir = tmp_dir("scoped");
        fs::create_dir_all(dir.join("guides/deep")).unwrap();
        fs::write(dir.join("top.md"), "# Top\n").unwrap();
        fs::write(dir.join("guides/one.md"), "# One\n").unwrap();
        fs::write(dir.join("guides/deep/two.md"), "# Two\n").unwrap();
        let addr = start_server(dir, None, None);

        let (status, body, _) = http_get(addr, "/guides", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("guides/one.md"));
        // A sibling outside the directory and a file nested below it are both
        // somebody else's listing.
        assert!(!body.contains("top.md"));
        assert!(!body.contains("guides/deep/two.md"));
    }

    #[test]
    fn the_root_listing_still_shows_the_whole_tree() {
        let addr = start_server(setup(), None, None);
        let (status, body, _) = http_get(addr, "/", "test-agent", None);
        assert_eq!(status, 200);
        assert!(body.contains("a.md"));
        assert!(body.contains("docs/b.md"));
    }

    #[test]
    fn files_under_filters_to_one_level() {
        let entry = |rel: &str| FileEntry {
            rel: rel.into(),
            size: 0,
            modified: SystemTime::UNIX_EPOCH,
        };
        let all = vec![
            entry("top.md"),
            entry("guides/one.md"),
            entry("guides/deep/two.md"),
            // A directory whose name merely starts with "guides" is not inside it.
            entry("guides-extra/three.md"),
        ];
        let rels = |under: &str| -> Vec<String> {
            files_under(&all, under).into_iter().map(|f| f.rel).collect()
        };
        assert_eq!(rels("guides"), vec!["guides/one.md".to_string()]);
        assert_eq!(rels("guides/deep"), vec!["guides/deep/two.md".to_string()]);
        assert_eq!(rels("").len(), all.len());
        assert!(rels("nowhere").is_empty());
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

    // ------------------------------------------------------- the agent surface

    #[test]
    fn the_agent_routes_do_not_exist_without_the_plugin() {
        // The default server must be exactly what it was before this feature.
        let addr = start_server(tmp_dir("noplugin"), None, None);
        let (status, resp) = http_post(addr, "/mcp", &[], None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        assert_eq!(status, 405, "{resp}");
        for path in ["/llms.txt", "/llms-full.txt", "/.well-known/mcp.json"] {
            let (status, _, _) = http_get(addr, path, "test-agent", None);
            assert_eq!(status, 404, "{path} must not exist without --plugin webmcp");
        }
    }

    #[test]
    fn tools_list_is_served_over_post() {
        let addr = start_server_with(tmp_dir("tools"), None, None, &["webmcp"]);
        let (status, resp) = http_post(
            addr,
            "/mcp",
            &[],
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        assert_eq!(status, 200, "{resp}");
        assert!(resp.to_ascii_lowercase().contains("content-type: application/json"));
        let body = body_of(&resp);
        for tool in ["search_docs", "read_doc", "list_docs", "get_outline"] {
            assert!(body.contains(tool), "{tool} missing from {body}");
        }
    }

    #[test]
    fn get_and_delete_on_the_endpoint_are_refused() {
        // 2026-07-28 removed the GET stream and the DELETE session teardown.
        let addr = start_server_with(tmp_dir("getmcp"), None, None, &["webmcp"]);
        let (status, resp, _) = http_get(addr, "/mcp", "test-agent", None);
        assert_eq!(status, 405);
        assert!(resp.contains("Allow: POST, OPTIONS"), "{resp}");

        let (status, _) = http_send(
            addr,
            "DELETE /mcp HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert_eq!(status, 405);
    }

    #[test]
    fn a_cors_preflight_is_answered_without_credentials() {
        // A browser never sends Authorization on a preflight, so requiring it
        // would lock every cross-origin agent out before it could authenticate.
        let addr = start_server_with(tmp_dir("pre"), Some("u"), Some("p"), &["webmcp"]);
        let (status, resp) = http_send(
            addr,
            "OPTIONS /mcp HTTP/1.1\r\nHost: localhost\r\nOrigin: https://agent.example\r\n\
             Access-Control-Request-Method: POST\r\n\r\n",
        );
        assert_eq!(status, 204, "{resp}");
        assert!(resp.contains("Access-Control-Allow-Origin: *"), "{resp}");
        assert!(resp.contains("MCP-Protocol-Version"), "{resp}");
    }

    #[test]
    fn the_endpoint_is_behind_the_same_auth_as_the_site() {
        let addr = start_server_with(tmp_dir("mcpauth"), Some("u"), Some("p"), &["webmcp"]);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let (status, _) = http_post(addr, "/mcp", &[], None, body);
        assert_eq!(status, 401, "no credentials, no tools");
        // "u:p"
        let (status, resp) = http_post(addr, "/mcp", &[], Some("dTpw"), body);
        assert_eq!(status, 200, "{resp}");
    }

    #[test]
    fn a_reply_carries_cors_headers_for_browser_agents() {
        let addr = start_server_with(tmp_dir("cors"), None, None, &["webmcp"]);
        let (_, resp) = http_post(
            addr,
            "/mcp",
            &[("Origin", "https://agent.example")],
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert!(resp.contains("Access-Control-Allow-Origin: *"), "{resp}");
    }

    #[test]
    fn an_oversized_body_never_reaches_the_parser() {
        let addr = start_server_with(tmp_dir("big"), None, None, &["webmcp"]);
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let (status, resp) = http_send(addr, &req);
        // The connection ends rather than the server reading a megabyte it has
        // already decided to reject.
        assert!(status == 0 || status >= 400, "got {status}: {resp}");
    }

    #[test]
    fn chunked_bodies_are_refused_rather_than_parsed() {
        let addr = start_server_with(tmp_dir("chunked"), None, None, &["webmcp"]);
        let (status, resp) = http_send(
            addr,
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        );
        assert!(status == 0 || status >= 400, "got {status}: {resp}");
    }

    #[test]
    fn llms_txt_is_generated_from_the_tree() {
        let addr = start_server_with(setup(), None, None, &["webmcp"]);
        let (status, resp, ct) = http_get(addr, "/llms.txt", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.to_ascii_lowercase().contains("text/plain"));
        let body = body_of(&resp);
        assert!(body.starts_with("# "), "{body}");
        assert!(body.contains("(/a.md)"), "{body}");
        assert!(body.contains("(/docs/b.md)"), "{body}");
        assert!(body.contains("/mcp"), "the endpoint is announced");
    }

    #[test]
    fn a_local_llms_txt_wins_over_the_generated_one() {
        let dir = tmp_dir("llmslocal");
        fs::write(dir.join("llms.txt"), "# Mine\n\n> Hand written.\n").unwrap();
        let addr = start_server_with(dir, None, None, &["webmcp"]);
        let (status, resp, _) = http_get(addr, "/llms.txt", "test-agent", None);
        assert_eq!(status, 200);
        assert_eq!(body_of(&resp), "# Mine\n\n> Hand written.\n");
    }

    #[test]
    fn llms_full_txt_carries_the_documents_themselves() {
        let addr = start_server_with(setup(), None, None, &["webmcp"]);
        let (status, resp, _) = http_get(addr, "/llms-full.txt", "test-agent", None);
        assert_eq!(status, 200);
        let body = body_of(&resp);
        assert!(body.contains("Hello **world**."), "{body}");
        assert!(body.contains("Source: `/docs/b.md`"), "{body}");
    }

    #[test]
    fn the_server_card_describes_the_endpoint() {
        let addr = start_server_with(tmp_dir("card"), None, None, &["webmcp"]);
        let (status, resp, ct) = http_get(addr, "/.well-known/mcp.json", "test-agent", None);
        assert_eq!(status, 200);
        assert!(ct.contains("application/json"), "{ct}");
        let v = crate::json::parse(body_of(&resp)).unwrap();
        let servers = v.get("mcp").unwrap().get("servers").unwrap().as_arr().unwrap();
        assert_eq!(servers[0].get("url").unwrap().as_str(), Some("/mcp"));
        assert_eq!(servers[0].get("transport").unwrap().as_str(), Some("streamable-http"));
        assert_eq!(v.get("mcp").unwrap().get("tools").unwrap().as_arr().unwrap().len(), 4);
    }

    #[test]
    fn the_webmcp_script_reaches_documents_and_the_listing() {
        let addr = start_server_with(setup(), None, None, &["webmcp"]);
        // A rendered document.
        let (_, doc, _) = http_get(addr, "/a.md", "test-agent", None);
        assert!(doc.contains("registerTool"), "missing on a document page");
        // And the page most visitors see first.
        let (_, listing, _) = http_get(addr, "/", "test-agent", None);
        assert!(listing.contains("registerTool"), "missing on the listing");
    }

    #[test]
    fn heading_ids_are_present_so_the_anchors_resolve() {
        let dir = tmp_dir("anchors");
        fs::write(dir.join("h.md"), "# Getting Started\n\ntext\n").unwrap();
        let addr = start_server_with(dir, None, None, &["webmcp"]);
        let (_, body, _) = http_get(addr, "/h.md", "test-agent", None);
        assert!(body.contains(r#"id="getting-started""#), "{body}");
    }

    #[test]
    fn documents_are_readable_through_the_endpoint() {
        let addr = start_server_with(setup(), None, None, &["webmcp"]);
        let body = call_tool(addr, "read_doc", r#"{"path":"a.md"}"#);
        assert!(body.contains("Hello"), "{body}");
    }

    #[test]
    fn the_endpoint_cannot_read_what_the_site_will_not_serve() {
        // The same refusals the router enforces, reached through a tool call.
        let dir = setup();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/config"), "token = hunter2\n").unwrap();
        fs::write(dir.join(".env"), "SECRET=hunter2\n").unwrap();
        let addr = start_server_with(dir, None, None, &["webmcp"]);

        for path in ["../../../etc/passwd", "/etc/passwd", ".git/config", ".env"] {
            let body = call_tool(addr, "read_doc", &format!(r#"{{"path":"{path}"}}"#));
            assert!(!body.contains("hunter2"), "{path} leaked: {body}");
            assert!(body.contains("isError") || body.contains("No such document"), "{body}");
        }
    }

    #[test]
    fn the_endpoint_lists_documents_as_resources() {
        let addr = start_server_with(setup(), None, None, &["webmcp"]);
        let (status, resp) = http_post(
            addr,
            "/mcp",
            &[],
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
        );
        assert_eq!(status, 200);
        let body = body_of(&resp);
        assert!(body.contains("serve-md:///a.md"), "{body}");
        assert!(body.contains("serve-md:///page.html"), "{body}");
        // A static asset is served but is not a document.
        assert!(!body.contains("ignore.txt"), "{body}");
    }

    #[test]
    fn a_fresh_server_notices_a_new_file() {
        let dir = tmp_dir("freshhttp");
        let names: Vec<String> = vec!["webmcp".to_string()];
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 0,
            dir: dir.clone(),
            user: None,
            pass: None,
            no_open: true,
            verbose: false,
            plugins: plugin::Set::resolve(&names).unwrap(),
            fresh: true,
            fresh_interval: Duration::from_millis(50),
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let _ = serve_on(cfg, listener);
        });
        std::thread::sleep(Duration::from_millis(30));

        fs::write(dir.join("late.md"), "# Late\n").unwrap();
        let mut seen = false;
        for _ in 0..100 {
            let (_, body, _) = http_get(addr, "/", "curl", None);
            if body.contains("late.md") {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(seen, "the watcher never surfaced late.md in the listing");
    }
}
