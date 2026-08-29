//! The tools and resources the MCP endpoint exposes.
//!
//! Four tools, chosen so an agent can work the way a person would: find the
//! relevant passage, look at the shape of a document before committing to
//! reading it, then read it.
//!
//! | Tool | For |
//! |---|---|
//! | `search_docs` | Finding which documents mention something, and where |
//! | `get_outline` | Seeing a long document's headings without reading it |
//! | `read_doc` | Reading one document, in Markdown, plain text or HTML |
//! | `list_docs` | The whole inventory |
//!
//! Every document is additionally published as an MCP *resource*, which is
//! what lets a person attach one directly — @-mentioning a file in a client —
//! rather than hoping the model calls a tool for it.
//!
//! Two rules bound everything here, and neither is enforced in this file
//! alone:
//!
//! - A path must be in the catalog snapshot, so nothing the listing hides can
//!   be named.
//! - A path must survive `http::resolve_document`, which re-applies the
//!   traversal, symlink and hidden-segment checks the website already uses.
//!   Duplicating that logic here would mean two implementations of the same
//!   security boundary, and eventually two behaviours.
//!
//! # References
//!
//! - Tool definitions and results:
//!   <https://modelcontextprotocol.io/specification/2026-07-28/server/tools>
//! - Resources:
//!   <https://modelcontextprotocol.io/specification/2026-07-28/server/resources>

use super::{error_reply, result_reply, Ctx, Reply, INVALID_PARAMS};
use crate::docmeta;
use crate::encoding::{percent_decode, percent_encode_path};
use crate::http;
use crate::json::Value;
use crate::render;
use crate::scanner::FileKind;
use crate::search;
use std::fs;
use std::path::Path;

/// The scheme resource URIs use. Not `file:`, which would name a location on
/// the server's disk that the client cannot and should not resolve.
const URI_SCHEME: &str = "serve-md:///";

/// Search results returned when the caller does not say.
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
/// Longest single matched line reproduced in a search result.
const MAX_SNIPPET: usize = 200;
/// Documents listed by `resources/list`. This transport has no pagination, so
/// a very large tree is truncated rather than turned into an enormous reply;
/// `list_docs` and `search_docs` still reach every document.
const MAX_RESOURCES: usize = 500;

// ---------------------------------------------------------------- definitions

fn schema<const N: usize>(properties: [(&str, Value); N], required: &[&str]) -> Value {
    Value::obj([
        ("type", Value::str("object")),
        ("properties", Value::obj(properties)),
        (
            "required",
            Value::Arr(required.iter().map(|r| Value::str(*r)).collect()),
        ),
    ])
}

fn prop(ty: &str, description: &str) -> Value {
    Value::obj([
        ("type", Value::str(ty)),
        ("description", Value::str(description)),
    ])
}

/// The `tools/list` payload.
pub fn definitions() -> Vec<Value> {
    vec![
        Value::obj([
            ("name", Value::str("search_docs")),
            (
                "description",
                Value::str(
                    "Full-text search across every served document. Returns matching lines with \
                     their file, line number, and the heading each match sits under. Start here \
                     when you do not already know which document holds the answer.",
                ),
            ),
            (
                "inputSchema",
                schema(
                    [
                        ("query", prop("string", "Text to search for. Matched literally and case-insensitively unless `regex` is true.")),
                        ("limit", prop("integer", "Maximum matches to return (1-50, default 10).")),
                        ("regex", prop("boolean", "Treat the query as a regular expression instead of literal text. Default false.")),
                    ],
                    &["query"],
                ),
            ),
        ]),
        Value::obj([
            ("name", Value::str("read_doc")),
            (
                "description",
                Value::str(
                    "Read one document in full. Use `format` to choose Markdown source \
                     (default), rendered plain text, or HTML.",
                ),
            ),
            (
                "inputSchema",
                schema(
                    [
                        ("path", prop("string", "Path of the document, as reported by search_docs or list_docs (for example `guides/start.md`).")),
                        ("format", prop("string", "One of `markdown` (default), `text`, or `html`.")),
                    ],
                    &["path"],
                ),
            ),
        ]),
        Value::obj([
            ("name", Value::str("list_docs")),
            (
                "description",
                Value::str(
                    "List every served document with its title, size and last-modified time. \
                     Useful for getting oriented; prefer search_docs when looking for something \
                     specific.",
                ),
            ),
            ("inputSchema", schema([], &[])),
        ]),
        Value::obj([
            ("name", Value::str("get_outline")),
            (
                "description",
                Value::str(
                    "List a document's headings with their anchors, so a long document can be \
                     navigated without reading all of it. Each anchor resolves on the served \
                     page as `/path#anchor`.",
                ),
            ),
            (
                "inputSchema",
                schema(
                    [("path", prop("string", "Path of the document, for example `guides/start.md`."))],
                    &["path"],
                ),
            ),
        ]),
    ]
}

// -------------------------------------------------------------------- calling

/// A successful tool result, in both the shape a model reads and the shape a
/// program reads.
///
/// `content` is what the model actually sees, so it is written for reading —
/// not a JSON dump. `structuredContent` carries the same facts for clients
/// that would rather parse than pattern-match.
fn ok_result(text: String, structured: Value) -> Value {
    Value::obj([
        (
            "content",
            Value::Arr(vec![Value::obj([
                ("type", Value::str("text")),
                ("text", Value::Str(text)),
            ])]),
        ),
        ("structuredContent", structured),
        ("isError", Value::Bool(false)),
    ])
}

/// A tool that ran and failed — a missing document, no search tool installed.
///
/// Reported as a *result* with `isError`, not a JSON-RPC error: the model is
/// meant to read this and try something else, and a protocol-level error is
/// handled by the client before the model ever sees it.
fn failed(message: String) -> Value {
    Value::obj([
        (
            "content",
            Value::Arr(vec![Value::obj([
                ("type", Value::str("text")),
                ("text", Value::Str(message)),
            ])]),
        ),
        ("isError", Value::Bool(true)),
    ])
}

pub fn call(params: &Value, id: Value, ctx: &Ctx) -> Reply {
    let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
        return error_reply(200, "OK", id, INVALID_PARAMS, "tools/call requires a `name`", None);
    };
    let args = params.get("arguments").cloned().unwrap_or(Value::Obj(vec![]));

    let result = match name {
        "search_docs" => search_docs(&args, ctx),
        "read_doc" => read_doc(&args, ctx),
        "list_docs" => list_docs(ctx),
        "get_outline" => get_outline(&args, ctx),
        _ => {
            return error_reply(
                200,
                "OK",
                id,
                INVALID_PARAMS,
                &format!("Unknown tool: {name}"),
                None,
            );
        }
    };
    result_reply(id, result)
}

// ---------------------------------------------------------------- path access

/// Normalises a caller-supplied path and refuses anything the site would not
/// serve at that path.
///
/// Accepts `guides/start.md` and `/guides/start.md` alike, and percent-decodes
/// so a path copied out of a URL still works.
fn load(ctx: &Ctx, raw: &str) -> Result<(FileKind, String, String), String> {
    let decoded = percent_decode(raw).unwrap_or_else(|_| raw.to_string());
    let rel = decoded.trim_start_matches('/').to_string();

    // The catalog first: it is the same list the website's listing page shows,
    // so a path absent from it is a path this server does not publish.
    if !ctx.snap.contains(&rel) {
        return Err(format!("No such document: {rel}"));
    }
    // Then the router's own resolution, which is where traversal, symlink and
    // hidden-segment refusal actually live.
    let Some((full, kind)) = http::resolve_document(ctx.root, &rel) else {
        return Err(format!("No such document: {rel}"));
    };
    let src = fs::read_to_string(&full).map_err(|e| format!("Could not read {rel}: {e}"))?;
    Ok((kind, rel, src))
}

fn url_for(rel: &str, anchor: Option<&str>) -> String {
    let base = percent_encode_path(&format!("/{rel}"));
    match anchor {
        Some(a) if !a.is_empty() => format!("{base}#{a}"),
        _ => base,
    }
}

// ----------------------------------------------------------------- the tools

fn search_docs(args: &Value, ctx: &Ctx) -> Value {
    let Some(query) = args.get("query").and_then(|q| q.as_str()) else {
        return failed("search_docs requires a `query`".to_string());
    };
    let limit = args
        .get("limit")
        .and_then(|l| l.as_i64())
        .map(|n| (n.max(1) as usize).min(MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);
    let regex = args.get("regex").and_then(|r| r.as_bool()).unwrap_or(false);

    let Some(engine) = ctx.engine else {
        return failed(search::Error::NoEngine.to_string());
    };
    let hits = match search::run(engine, ctx.root, ctx.snap, query, regex) {
        Ok(h) => h,
        Err(e) => return failed(e.to_string()),
    };
    if hits.is_empty() {
        return ok_result(
            format!("No matches for {query:?}."),
            Value::obj([("matches", Value::Arr(vec![])), ("total", Value::int(0))]),
        );
    }

    // Group by document, so each file is read and parsed once however many
    // times it matched.
    let mut by_file: Vec<(String, Vec<u64>)> = Vec::new();
    for hit in &hits {
        match by_file.iter_mut().find(|(rel, _)| *rel == hit.rel) {
            Some((_, lines)) => lines.push(hit.line),
            None => by_file.push((hit.rel.clone(), vec![hit.line])),
        }
    }
    // Most matches first: the document that mentions a term repeatedly is
    // usually the one about it. Ties break on path so the order is stable.
    by_file.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    let options = ctx.plugins.options();
    let mut text = String::new();
    let mut entries: Vec<Value> = Vec::new();
    let mut shown = 0usize;

    for (rel, lines) in &by_file {
        if shown >= limit {
            break;
        }
        let Ok(src) = fs::read_to_string(ctx.root.join(rel)) else {
            continue;
        };
        let source_lines: Vec<&str> = src.lines().collect();
        let headings = docmeta::headings(&src, &options);

        text.push_str(&format!("\n/{rel}\n"));
        for line_no in lines {
            if shown >= limit {
                break;
            }
            let idx = (*line_no as usize).saturating_sub(1);
            let Some(raw) = source_lines.get(idx) else { continue };
            let snippet = clip(raw.trim());
            let heading = docmeta::heading_at(&headings, *line_no);
            let anchor = heading.map(|h| h.anchor.as_str());

            match heading {
                Some(h) => text.push_str(&format!("  {line_no}: {snippet}\n      under “{}” — {}\n", h.text, url_for(rel, anchor))),
                None => text.push_str(&format!("  {line_no}: {snippet}\n      {}\n", url_for(rel, None))),
            }

            entries.push(Value::obj([
                ("path", Value::str(rel.as_str())),
                ("line", Value::int(*line_no as i64)),
                ("text", Value::str(snippet)),
                (
                    "heading",
                    heading.map(|h| Value::str(h.text.as_str())).unwrap_or(Value::Null),
                ),
                (
                    "anchor",
                    anchor.map(Value::str).unwrap_or(Value::Null),
                ),
                ("url", Value::str(url_for(rel, anchor))),
            ]));
            shown += 1;
        }
    }

    let header = format!(
        "{} match{} for {query:?} across {} document{}{}\n",
        hits.len(),
        if hits.len() == 1 { "" } else { "es" },
        by_file.len(),
        if by_file.len() == 1 { "" } else { "s" },
        if hits.len() > shown {
            format!(" (showing the first {shown})")
        } else {
            String::new()
        }
    );

    ok_result(
        format!("{header}{text}"),
        Value::obj([
            ("matches", Value::Arr(entries)),
            ("total", Value::int(hits.len() as i64)),
            ("returned", Value::int(shown as i64)),
        ]),
    )
}

fn read_doc(args: &Value, ctx: &Ctx) -> Value {
    let Some(path) = args.get("path").and_then(|p| p.as_str()) else {
        return failed("read_doc requires a `path`".to_string());
    };
    let format = args.get("format").and_then(|f| f.as_str()).unwrap_or("markdown");
    let (kind, rel, src) = match load(ctx, path) {
        Ok(v) => v,
        Err(e) => return failed(e),
    };

    let body = match format {
        "markdown" => render::to_markdown(kind, &src),
        "text" => render::to_text(kind, &src, ctx.plugins),
        "html" => render::to_html(kind, &src, ctx.plugins).html,
        other => {
            return failed(format!(
                "Unknown format {other:?}. Use `markdown`, `text`, or `html`."
            ))
        }
    };

    ok_result(
        body.clone(),
        Value::obj([
            ("path", Value::str(rel.as_str())),
            ("format", Value::str(format)),
            ("url", Value::str(url_for(&rel, None))),
            ("content", Value::Str(body)),
        ]),
    )
}

fn list_docs(ctx: &Ctx) -> Value {
    let options = ctx.plugins.options();
    let mut text = format!("{} document(s):\n", ctx.snap.len());
    let mut entries = Vec::new();

    for f in &ctx.snap.files {
        let kind = FileKind::from_path(Path::new(&f.rel));
        let title = if matches!(kind, FileKind::Markdown | FileKind::Html) {
            fs::read_to_string(ctx.root.join(&f.rel))
                .ok()
                .and_then(|src| docmeta::title(&src, &options))
        } else {
            None
        };
        match &title {
            Some(t) => text.push_str(&format!("  /{} — {t}\n", f.rel)),
            None => text.push_str(&format!("  /{}\n", f.rel)),
        }
        entries.push(Value::obj([
            ("path", Value::str(f.rel.as_str())),
            ("title", title.map(Value::Str).unwrap_or(Value::Null)),
            ("size", Value::int(f.size as i64)),
            ("modified", Value::str(crate::page::format_time(f.modified))),
            ("url", Value::str(url_for(&f.rel, None))),
        ]));
    }

    ok_result(
        text,
        Value::obj([
            ("documents", Value::Arr(entries)),
            ("total", Value::int(ctx.snap.len() as i64)),
        ]),
    )
}

fn get_outline(args: &Value, ctx: &Ctx) -> Value {
    let Some(path) = args.get("path").and_then(|p| p.as_str()) else {
        return failed("get_outline requires a `path`".to_string());
    };
    let (_, rel, src) = match load(ctx, path) {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    let headings = docmeta::headings(&src, &ctx.plugins.options());
    if headings.is_empty() {
        return ok_result(
            format!("/{rel} has no headings."),
            Value::obj([
                ("path", Value::str(rel.as_str())),
                ("headings", Value::Arr(vec![])),
            ]),
        );
    }

    let mut text = format!("Outline of /{rel}:\n");
    let mut entries = Vec::new();
    for h in &headings {
        // Indented by level, so the shape of the document is visible at a
        // glance rather than having to be reconstructed from the numbers.
        text.push_str(&format!(
            "{}{} — {}\n",
            "  ".repeat(h.level.saturating_sub(1) as usize),
            h.text,
            url_for(&rel, Some(&h.anchor))
        ));
        entries.push(Value::obj([
            ("level", Value::int(h.level as i64)),
            ("text", Value::str(h.text.as_str())),
            ("anchor", Value::str(h.anchor.as_str())),
            ("line", Value::int(h.line as i64)),
            ("url", Value::str(url_for(&rel, Some(&h.anchor)))),
        ]));
    }

    ok_result(
        text,
        Value::obj([
            ("path", Value::str(rel.as_str())),
            ("headings", Value::Arr(entries)),
        ]),
    )
}

// ------------------------------------------------------------------ resources

fn mime_for(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Markdown => "text/markdown",
        FileKind::Html => "text/html",
        FileKind::Static => "application/octet-stream",
    }
}

pub fn resource_list(ctx: &Ctx) -> Vec<Value> {
    let options = ctx.plugins.options();
    let mut out = Vec::new();
    for f in &ctx.snap.files {
        if out.len() >= MAX_RESOURCES {
            break;
        }
        let kind = FileKind::from_path(Path::new(&f.rel));
        if !matches!(kind, FileKind::Markdown | FileKind::Html) {
            continue;
        }
        let src = fs::read_to_string(ctx.root.join(&f.rel)).ok();
        let title = src
            .as_deref()
            .and_then(|s| docmeta::title(s, &options))
            .unwrap_or_else(|| f.rel.clone());
        let description = src.as_deref().and_then(|s| docmeta::summary(s, &options));
        out.push(Value::obj([
            ("uri", Value::str(format!("{URI_SCHEME}{}", f.rel))),
            ("name", Value::Str(title)),
            (
                "description",
                description.map(Value::Str).unwrap_or(Value::Null),
            ),
            ("mimeType", Value::str(mime_for(kind))),
        ]));
    }
    out
}

pub fn resource_read(params: &Value, ctx: &Ctx) -> Result<Value, String> {
    let Some(uri) = params.get("uri").and_then(|u| u.as_str()) else {
        return Err("resources/read requires a `uri`".to_string());
    };
    let Some(rel) = uri.strip_prefix(URI_SCHEME) else {
        return Err(format!("Unsupported resource URI: {uri}"));
    };
    let (kind, rel, src) = load(ctx, rel)?;
    Ok(Value::obj([(
        "contents",
        Value::Arr(vec![Value::obj([
            ("uri", Value::str(format!("{URI_SCHEME}{rel}"))),
            ("mimeType", Value::str(mime_for(kind))),
            ("text", Value::Str(src)),
        ])]),
    )]))
}

/// Truncates a reproduced source line on a character boundary.
fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_SNIPPET {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_SNIPPET).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::json;
    use crate::plugin;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-tools-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn fixture() -> PathBuf {
        let d = tmp("fix");
        fs::create_dir_all(d.join("guides")).unwrap();
        fs::write(d.join("README.md"), "# Widgets\n\nAll about widgets.\n").unwrap();
        fs::write(
            d.join("guides/start.md"),
            "# Start\n\nIntro.\n\n## Install\n\nRun it.\n\n## Configure\n\nEdit the file.\n",
        )
        .unwrap();
        fs::write(d.join("logo.png"), "binary-ish").unwrap();
        d
    }

    /// Calls a tool and returns its result value.
    fn tool(dir: &Path, name: &str, args: Value) -> Value {
        let catalog = Catalog::scan(dir).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: dir, snap: &snap, plugins: &plugins, engine: search::detect() };
        let params = Value::obj([("name", Value::str(name)), ("arguments", args)]);
        let reply = call(&params, Value::int(1), &ctx);
        let parsed = json::parse(&reply.body).unwrap();
        parsed.get("result").cloned().unwrap_or_else(|| parsed.clone())
    }

    fn text_of(result: &Value) -> String {
        result
            .get("content")
            .and_then(|c| c.as_arr())
            .and_then(|a| a.first())
            .and_then(|f| f.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn is_error(result: &Value) -> bool {
        result.get("isError").and_then(|e| e.as_bool()).unwrap_or(false)
    }

    #[test]
    fn every_tool_declares_a_name_description_and_schema() {
        let defs = definitions();
        assert_eq!(defs.len(), 4);
        for d in &defs {
            assert!(d.get("name").and_then(|n| n.as_str()).is_some());
            let desc = d.get("description").and_then(|n| n.as_str()).unwrap();
            assert!(desc.len() > 30, "descriptions are what the model routes on");
            let schema = d.get("inputSchema").unwrap();
            assert_eq!(schema.get("type").unwrap().as_str(), Some("object"));
            assert!(schema.get("properties").is_some());
        }
        let names: Vec<&str> = defs
            .iter()
            .filter_map(|d| d.get("name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["search_docs", "read_doc", "list_docs", "get_outline"]);
    }

    #[test]
    fn read_doc_returns_the_source() {
        let d = fixture();
        let r = tool(&d, "read_doc", Value::obj([("path", Value::str("README.md"))]));
        assert!(!is_error(&r));
        assert!(text_of(&r).contains("# Widgets"));
        assert_eq!(
            r.get("structuredContent").unwrap().get("url").unwrap().as_str(),
            Some("/README.md")
        );
    }

    #[test]
    fn read_doc_accepts_a_leading_slash_and_percent_encoding() {
        let d = tmp("enc");
        fs::write(d.join("my file.md"), "# Spaces\n").unwrap();
        for path in ["my file.md", "/my file.md", "/my%20file.md"] {
            let r = tool(&d, "read_doc", Value::obj([("path", Value::str(path))]));
            assert!(!is_error(&r), "{path} should resolve");
            assert!(text_of(&r).contains("# Spaces"));
        }
    }

    #[test]
    fn read_doc_honours_the_format_argument() {
        let d = fixture();
        let html = tool(
            &d,
            "read_doc",
            Value::obj([("path", Value::str("README.md")), ("format", Value::str("html"))]),
        );
        assert!(text_of(&html).contains("<h1"));

        let text = tool(
            &d,
            "read_doc",
            Value::obj([("path", Value::str("README.md")), ("format", Value::str("text"))]),
        );
        assert!(text_of(&text).contains("WIDGETS"));

        let bad = tool(
            &d,
            "read_doc",
            Value::obj([("path", Value::str("README.md")), ("format", Value::str("pdf"))]),
        );
        assert!(is_error(&bad));
    }

    #[test]
    fn traversal_and_hidden_paths_are_refused() {
        let d = fixture();
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/config"), "[core]\nsecret = yes\n").unwrap();
        fs::write(d.join(".env"), "TOKEN=hunter2\n").unwrap();

        for path in [
            "../../../etc/passwd",
            "/etc/passwd",
            ".git/config",
            ".env",
            "guides/../.env",
            "missing.md",
        ] {
            let r = tool(&d, "read_doc", Value::obj([("path", Value::str(path))]));
            assert!(is_error(&r), "{path} must not be readable");
            assert!(!text_of(&r).contains("hunter2"));
            assert!(!text_of(&r).contains("secret"));

            let o = tool(&d, "get_outline", Value::obj([("path", Value::str(path))]));
            assert!(is_error(&o), "{path} must not be outlined");
        }
    }

    #[test]
    fn get_outline_lists_headings_with_resolvable_anchors() {
        let d = fixture();
        let r = tool(&d, "get_outline", Value::obj([("path", Value::str("guides/start.md"))]));
        assert!(!is_error(&r));
        let headings = r
            .get("structuredContent")
            .unwrap()
            .get("headings")
            .unwrap()
            .as_arr()
            .unwrap()
            .to_vec();
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].get("text").unwrap().as_str(), Some("Start"));
        assert_eq!(headings[1].get("anchor").unwrap().as_str(), Some("install"));
        assert_eq!(
            headings[1].get("url").unwrap().as_str(),
            Some("/guides/start.md#install")
        );
        assert_eq!(headings[1].get("level").unwrap().as_i64(), Some(2));
    }

    #[test]
    fn get_outline_on_a_document_without_headings_is_not_an_error() {
        let d = tmp("flat");
        fs::write(d.join("flat.md"), "just prose\n").unwrap();
        let r = tool(&d, "get_outline", Value::obj([("path", Value::str("flat.md"))]));
        assert!(!is_error(&r));
        assert!(text_of(&r).contains("no headings"));
    }

    #[test]
    fn list_docs_reports_every_served_file_with_its_title() {
        let d = fixture();
        let r = tool(&d, "list_docs", Value::Obj(vec![]));
        let docs = r
            .get("structuredContent")
            .unwrap()
            .get("documents")
            .unwrap()
            .as_arr()
            .unwrap()
            .to_vec();
        assert_eq!(docs.len(), 3, "including the static asset");
        let readme = docs.iter().find(|d| d.get("path").unwrap().as_str() == Some("README.md")).unwrap();
        assert_eq!(readme.get("title").unwrap().as_str(), Some("Widgets"));
        // A static file has no title rather than a wrong one.
        let logo = docs.iter().find(|d| d.get("path").unwrap().as_str() == Some("logo.png")).unwrap();
        assert!(logo.get("title").unwrap().is_null());
    }

    #[test]
    fn unknown_tools_are_a_protocol_error_not_a_result() {
        let d = fixture();
        let catalog = Catalog::scan(&d).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: &d, snap: &snap, plugins: &plugins, engine: None };
        let params = Value::obj([("name", Value::str("no_such_tool"))]);
        let reply = call(&params, Value::int(1), &ctx);
        let v = json::parse(&reply.body).unwrap();
        assert_eq!(
            v.get("error").unwrap().get("code").unwrap().as_i64(),
            Some(INVALID_PARAMS)
        );
    }

    #[test]
    fn search_without_an_engine_says_what_to_install() {
        let d = fixture();
        let catalog = Catalog::scan(&d).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: &d, snap: &snap, plugins: &plugins, engine: None };
        let params = Value::obj([
            ("name", Value::str("search_docs")),
            ("arguments", Value::obj([("query", Value::str("widgets"))])),
        ]);
        let reply = call(&params, Value::int(1), &ctx);
        let result = json::parse(&reply.body).unwrap().get("result").cloned().unwrap();
        assert!(is_error(&result));
        assert!(text_of(&result).contains("ripgrep"));
    }

    #[test]
    fn search_finds_text_and_names_the_enclosing_heading() {
        // Skipped where no search tool exists, rather than failing: CI images
        // vary and this is the one test that needs an external binary.
        let Some(_) = search::detect() else { return };
        let d = fixture();
        let r = tool(
            &d,
            "search_docs",
            Value::obj([("query", Value::str("Run it"))]),
        );
        assert!(!is_error(&r), "{}", text_of(&r));
        let matches = r
            .get("structuredContent")
            .unwrap()
            .get("matches")
            .unwrap()
            .as_arr()
            .unwrap()
            .to_vec();
        assert!(!matches.is_empty(), "expected a hit for 'Run it'");
        let hit = &matches[0];
        assert_eq!(hit.get("path").unwrap().as_str(), Some("guides/start.md"));
        assert_eq!(hit.get("heading").unwrap().as_str(), Some("Install"));
        assert_eq!(
            hit.get("url").unwrap().as_str(),
            Some("/guides/start.md#install")
        );
    }

    #[test]
    fn search_never_reports_a_file_the_site_does_not_serve() {
        let Some(_) = search::detect() else { return };
        let d = fixture();
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/config"), "widgets are secret\n").unwrap();
        fs::write(d.join(".env"), "widgets=secret\n").unwrap();

        let r = tool(&d, "search_docs", Value::obj([("query", Value::str("widgets"))]));
        let body = text_of(&r);
        assert!(!body.contains(".git"), "{body}");
        assert!(!body.contains(".env"), "{body}");
        assert!(!body.contains("secret"), "{body}");
    }

    #[test]
    fn search_respects_its_limit() {
        let Some(_) = search::detect() else { return };
        let d = tmp("many");
        for i in 0..20 {
            fs::write(d.join(format!("{i:02}.md")), "# T\n\nneedle here\n").unwrap();
        }
        let r = tool(
            &d,
            "search_docs",
            Value::obj([("query", Value::str("needle")), ("limit", Value::int(3))]),
        );
        let returned = r
            .get("structuredContent")
            .unwrap()
            .get("returned")
            .unwrap()
            .as_i64();
        assert_eq!(returned, Some(3));
    }

    #[test]
    fn a_search_with_no_matches_is_a_normal_result() {
        let Some(_) = search::detect() else { return };
        let d = fixture();
        let r = tool(
            &d,
            "search_docs",
            Value::obj([("query", Value::str("zzzznotpresentzzzz"))]),
        );
        assert!(!is_error(&r));
        assert!(text_of(&r).contains("No matches"));
    }

    #[test]
    fn resources_cover_documents_but_not_static_assets() {
        let d = fixture();
        let catalog = Catalog::scan(&d).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: &d, snap: &snap, plugins: &plugins, engine: None };

        let list = resource_list(&ctx);
        assert_eq!(list.len(), 2);
        let uris: Vec<&str> = list
            .iter()
            .filter_map(|r| r.get("uri").and_then(|u| u.as_str()))
            .collect();
        assert!(uris.contains(&"serve-md:///README.md"));
        assert!(uris.contains(&"serve-md:///guides/start.md"));
        assert!(!uris.iter().any(|u| u.contains("logo.png")));
        assert_eq!(
            list.iter()
                .find(|r| r.get("uri").unwrap().as_str() == Some("serve-md:///README.md"))
                .unwrap()
                .get("mimeType")
                .unwrap()
                .as_str(),
            Some("text/markdown")
        );
    }

    #[test]
    fn a_resource_round_trips_from_list_to_read() {
        let d = fixture();
        let catalog = Catalog::scan(&d).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: &d, snap: &snap, plugins: &plugins, engine: None };

        let uri = resource_list(&ctx)[0].get("uri").unwrap().as_str().unwrap().to_string();
        let read = resource_read(&Value::obj([("uri", Value::str(uri.clone()))]), &ctx).unwrap();
        let contents = read.get("contents").unwrap().as_arr().unwrap();
        assert_eq!(contents[0].get("uri").unwrap().as_str(), Some(uri.as_str()));
        assert!(contents[0].get("text").unwrap().as_str().unwrap().contains('#'));
    }

    #[test]
    fn a_resource_uri_cannot_name_something_unserved() {
        let d = fixture();
        fs::write(d.join(".env"), "TOKEN=hunter2\n").unwrap();
        let catalog = Catalog::scan(&d).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: &d, snap: &snap, plugins: &plugins, engine: None };

        for uri in [
            "serve-md:///.env",
            "serve-md:///../../etc/passwd",
            "file:///etc/passwd",
            "serve-md:///missing.md",
        ] {
            let r = resource_read(&Value::obj([("uri", Value::str(uri))]), &ctx);
            assert!(r.is_err(), "{uri} must not be readable");
        }
    }

    #[test]
    fn long_lines_are_clipped_on_a_character_boundary() {
        let long = "é".repeat(MAX_SNIPPET + 50);
        let out = clip(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), MAX_SNIPPET + 1);
        assert_eq!(clip("short"), "short");
    }
}
