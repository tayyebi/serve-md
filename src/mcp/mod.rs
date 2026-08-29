//! The Model Context Protocol endpoint.
//!
//! One `POST /mcp` carrying one JSON-RPC message, answered with one JSON
//! object. That is the whole transport: no session, no handshake, no stream.
//!
//! That shape is not a simplification — it is what the protocol now specifies.
//! Revision 2026-07-28 made the core stateless, removing the
//! `initialize`/`initialized` handshake and the `Mcp-Session-Id` header, and
//! dropping the GET stream endpoint. Each request carries its own protocol
//! version and client capabilities in `_meta`. A server that answers every
//! request independently is conformant, which is exactly what a
//! thread-per-connection server with no shared mutable state can offer.
//!
//! # Two eras
//!
//! Revisions 2025-03-26 through 2025-11-25 opened with `initialize` and
//! expected a session. Most clients in the field still do. Refusing them would
//! make this endpoint spec-correct and useless, so `initialize` is answered
//! too — without minting a session id, which the older revisions permit
//! (assigning one is a MAY, never a MUST). [`is_modern`] decides which era a
//! request belongs to, and only the differences in required behaviour are
//! conditioned on it.
//!
//! # References
//!
//! - MCP 2026-07-28, Streamable HTTP binding:
//!   <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http>
//! - MCP 2026-07-28, release notes (stateless core, header routing,
//!   cacheable lists): <https://blog.modelcontextprotocol.io/posts/2026-07-28/>
//! - MCP 2025-06-18, Streamable HTTP (the era most clients still speak):
//!   <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>
//! - JSON-RPC 2.0, including the reserved error codes:
//!   <https://www.jsonrpc.org/specification>

pub mod tools;

use crate::catalog::Snapshot;
use crate::encoding::base64_decode;
use crate::json::{self, Value};
use crate::plugin;
use crate::search;
use std::path::Path;

/// The newest revision this server implements.
pub const PROTOCOL_LATEST: &str = "2026-07-28";

/// Every revision this server will answer, newest first. Advertised verbatim
/// in an `UnsupportedProtocolVersionError`.
pub const SUPPORTED: &[&str] = &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"];

/// What to assume when `MCP-Protocol-Version` is absent.
///
/// 2026-07-28 §"Protocol Version Header": a server that supports clients older
/// than 2025-06-18 — which did not define the header — MAY treat a request
/// without it as 2025-03-26. serve-md does, because a missing header is far
/// more often an older client than a malformed modern one.
const ASSUMED_WHEN_ABSENT: &str = "2025-03-26";

// JSON-RPC 2.0 §5.1, plus the MCP-allocated code for header validation.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// MCP 2026-07-28 `HeaderMismatch`, from the range the spec reserves for
/// protocol-defined errors.
const HEADER_MISMATCH: i64 = -32020;

/// Whether a revision uses per-request metadata rather than an `initialize`
/// handshake.
///
/// Revision names are ISO dates of fixed width, so lexicographic order is
/// chronological order and a plain string comparison is a date comparison.
fn is_modern(version: &str) -> bool {
    version >= PROTOCOL_LATEST
}

/// Everything a request needs to be answered. Borrowed for the life of one
/// request, so nothing here outlives the snapshot it was taken against.
pub struct Ctx<'a> {
    pub root: &'a Path,
    pub snap: &'a Snapshot,
    pub plugins: &'a plugin::Set,
    /// `None` when no search tool was found on `PATH`; only `search_docs`
    /// cares.
    pub engine: Option<search::Engine>,
}

/// An HTTP answer: status, reason phrase, and a body that is either JSON or
/// empty.
pub struct Reply {
    pub status: u16,
    pub reason: &'static str,
    pub body: String,
}

impl Reply {
    fn json(status: u16, reason: &'static str, v: Value) -> Reply {
        Reply { status, reason, body: json::write(&v) }
    }

    /// 202 with no body: the answer to a notification. 2026-07-28
    /// §"Sending Messages" item 5 makes this a MUST.
    fn accepted() -> Reply {
        Reply { status: 202, reason: "Accepted", body: String::new() }
    }
}

fn error_object(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut fields = vec![
        ("code".to_string(), Value::int(code)),
        ("message".to_string(), Value::str(message)),
    ];
    if let Some(d) = data {
        fields.push(("data".to_string(), d));
    }
    Value::Obj(fields)
}

fn error_reply(status: u16, reason: &'static str, id: Value, code: i64, message: &str, data: Option<Value>) -> Reply {
    Reply::json(
        status,
        reason,
        Value::obj([
            ("jsonrpc", Value::str("2.0")),
            ("id", id),
            ("error", error_object(code, message, data)),
        ]),
    )
}

fn result_reply(id: Value, result: Value) -> Reply {
    Reply::json(
        200,
        "OK",
        Value::obj([
            ("jsonrpc", Value::str("2.0")),
            ("id", id),
            ("result", result),
        ]),
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Decodes the `=?base64?…?=` sentinel the transport uses for header values
/// that are not plain ASCII, and passes anything else through.
///
/// 2026-07-28 §"Value Encoding": the markers are lowercase and case-sensitive,
/// and a server MUST decode before comparing a header to the body.
fn decode_header_value(v: &str) -> Option<String> {
    let Some(inner) = v.strip_prefix("=?base64?").and_then(|s| s.strip_suffix("?=")) else {
        return Some(v.to_string());
    };
    let bytes = base64_decode(inner).ok()?;
    String::from_utf8(bytes).ok()
}

/// Answers one POST to the MCP endpoint.
pub fn handle(body: &str, headers: &[(String, String)], ctx: &Ctx) -> Reply {
    let msg = match json::parse(body) {
        Ok(v) => v,
        Err(e) => {
            // 2026-07-28 §"Sending Messages" item 5: a body the server cannot
            // accept gets an HTTP error status, and the body MAY be a JSON-RPC
            // error response with no id.
            return error_reply(
                400,
                "Bad Request",
                Value::Null,
                PARSE_ERROR,
                &format!("Parse error: {e}"),
                None,
            );
        }
    };

    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
        return error_reply(400, "Bad Request", id, INVALID_REQUEST, "Invalid Request: no method", None);
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Obj(vec![]));

    let version = match negotiate_version(&msg, headers, &id) {
        Ok(v) => v,
        Err(reply) => return *reply,
    };
    if let Some(reply) = validate_routing_headers(method, &params, headers, &id, version) {
        return *reply;
    }

    // A message with no `id` is a notification: acknowledged, never answered.
    // An explicit `"id": null` counts as one too — some clients spell it that
    // way — and `notifications/*` is checked by name as well, because others
    // send a real id on them regardless.
    if msg.get("id").is_none_or(Value::is_null) || method.starts_with("notifications/") {
        return Reply::accepted();
    }

    dispatch(method, &params, id, version, ctx)
}

/// Resolves the protocol version for this request and enforces the two rules
/// that carry an HTTP status of their own.
fn negotiate_version(
    msg: &Value,
    headers: &[(String, String)],
    id: &Value,
) -> Result<&'static str, Box<Reply>> {
    let from_header = header(headers, "MCP-Protocol-Version");
    let from_body = msg
        .get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(|v| v.as_str());

    // 2026-07-28 §"Protocol Version Header": the header MUST match the `_meta`
    // field, and a mismatch is a 400 with HeaderMismatch. Checked before the
    // version is trusted, since the two disagreeing is precisely the confusion
    // the rule exists to prevent.
    if let (Some(h), Some(b)) = (from_header, from_body) {
        if h != b {
            return Err(Box::new(error_reply(
                400,
                "Bad Request",
                id.clone(),
                HEADER_MISMATCH,
                &format!(
                    "Header mismatch: MCP-Protocol-Version header value '{h}' does not match body value '{b}'"
                ),
                None,
            )));
        }
    }

    let requested = from_header.or(from_body).unwrap_or(ASSUMED_WHEN_ABSENT);
    match SUPPORTED.iter().find(|v| **v == requested) {
        Some(v) => Ok(*v),
        None => Err(Box::new(error_reply(
            400,
            "Bad Request",
            id.clone(),
            INVALID_REQUEST,
            &format!("Unsupported protocol version: {requested}"),
            Some(Value::obj([
                ("supported", Value::Arr(SUPPORTED.iter().map(|v| Value::str(*v)).collect())),
                ("requested", Value::str(requested)),
            ])),
        ))),
    }
}

/// Enforces the `Mcp-Method` / `Mcp-Name` mirroring rules.
///
/// 2026-07-28 §"Request Metadata" makes both headers REQUIRED for compliance
/// and requires a server that reads the body to reject any disagreement with
/// `-32020`, so that a gateway routing on the header and a server executing on
/// the body can never act on different values.
///
/// Presence is required only of modern clients. Older revisions never defined
/// these headers, and demanding them of a 2025-06-18 client would reject every
/// conforming request it can make. A *mismatch* is rejected in either era,
/// since that is a genuine disagreement rather than an absent convention.
fn validate_routing_headers(
    method: &str,
    params: &Value,
    headers: &[(String, String)],
    id: &Value,
    version: &str,
) -> Option<Box<Reply>> {
    let reject = |msg: String| {
        Some(Box::new(error_reply(
            400,
            "Bad Request",
            id.clone(),
            HEADER_MISMATCH,
            &msg,
            None,
        )))
    };

    match header(headers, "Mcp-Method") {
        Some(h) if h != method => {
            return reject(format!(
                "Header mismatch: Mcp-Method header value '{h}' does not match body value '{method}'"
            ));
        }
        None if is_modern(version) => {
            return reject("Header mismatch: Mcp-Method is required".to_string());
        }
        _ => {}
    }

    // `Mcp-Name` mirrors `params.name` for tool and prompt calls, and
    // `params.uri` for resource reads.
    let named = match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(|n| n.as_str()),
        "resources/read" => params.get("uri").and_then(|u| u.as_str()),
        _ => None,
    };
    let expected = named?;

    match header(headers, "Mcp-Name") {
        Some(raw) => {
            let decoded = decode_header_value(raw);
            match decoded {
                Some(d) if d == expected => {}
                Some(d) => {
                    return reject(format!(
                        "Header mismatch: Mcp-Name header value '{d}' does not match body value '{expected}'"
                    ));
                }
                None => return reject("Header mismatch: Mcp-Name is not valid base64".to_string()),
            }
        }
        None if is_modern(version) => {
            return reject("Header mismatch: Mcp-Name is required for this method".to_string());
        }
        _ => {}
    }
    None
}

fn dispatch(method: &str, params: &Value, id: Value, version: &str, ctx: &Ctx) -> Reply {
    match method {
        "initialize" => result_reply(id, initialize_result(params)),
        "server/discover" => result_reply(id, discover_result()),
        "ping" => result_reply(id, Value::Obj(vec![])),
        "tools/list" => result_reply(id, listed(("tools", tools::definitions()), version)),
        "resources/list" => {
            result_reply(id, listed(("resources", tools::resource_list(ctx)), version))
        }
        "resources/read" => match tools::resource_read(params, ctx) {
            Ok(v) => result_reply(id, v),
            Err(msg) => error_reply(200, "OK", id, INVALID_PARAMS, &msg, None),
        },
        "tools/call" => tools::call(params, id, ctx),
        _ => {
            // 2026-07-28 §"Protocol Version Header": an unimplemented method
            // MUST be 404 with -32601, which is how a modern client tells this
            // server apart from a legacy one that has no MCP endpoint here.
            // Older clients read 404 as "wrong URL", so they get 200 instead
            // and read the error out of the body.
            let status = if is_modern(version) { (404, "Not Found") } else { (200, "OK") };
            error_reply(
                status.0,
                status.1,
                id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
                None,
            )
        }
    }
}

/// Wraps a list result, adding the cache hints 2026-07-28 introduced for
/// `tools`, `prompts` and `resources` listings. Both lists are derived from a
/// catalog that only changes when the tree does, so a short client-side cache
/// is safe and saves a round trip per conversation turn.
fn listed((key, items): (&str, Vec<Value>), version: &str) -> Value {
    let mut fields = vec![(key.to_string(), Value::Arr(items))];
    if is_modern(version) {
        fields.push(("ttlMs".to_string(), Value::int(60_000)));
        fields.push(("cacheScope".to_string(), Value::str("session")));
    }
    Value::Obj(fields)
}

fn server_info() -> Value {
    Value::obj([
        ("name", Value::str("serve-md")),
        ("version", Value::str(env!("CARGO_PKG_VERSION"))),
    ])
}

fn capabilities() -> Value {
    // No `listChanged` on either: saying so would promise
    // `notifications/*/list_changed`, which needs a long-lived stream this
    // transport deliberately does not open.
    Value::obj([
        ("tools", Value::Obj(vec![])),
        ("resources", Value::Obj(vec![])),
    ])
}

const INSTRUCTIONS: &str = "\
These tools read a directory of Markdown and HTML documents served by serve-md.

Start with `search_docs` to find relevant passages, then `read_doc` to fetch a
whole document, or `get_outline` first if it is long and you only need one
section. `list_docs` gives the full inventory. Every document is also exposed
as a resource, so it can be attached directly.";

fn initialize_result(params: &Value) -> Value {
    // Echo the client's version when it is one this server speaks; otherwise
    // name the newest and let the client decide whether to continue.
    let asked = params.get("protocolVersion").and_then(|v| v.as_str());
    let agreed = asked
        .filter(|v| SUPPORTED.contains(v))
        .unwrap_or(PROTOCOL_LATEST);
    Value::obj([
        ("protocolVersion", Value::str(agreed)),
        ("capabilities", capabilities()),
        ("serverInfo", server_info()),
        ("instructions", Value::str(INSTRUCTIONS)),
    ])
}

/// The 2026-07-28 replacement for the handshake: capabilities up front, for
/// clients that want them, without any state being established.
fn discover_result() -> Value {
    Value::obj([
        ("protocolVersion", Value::str(PROTOCOL_LATEST)),
        (
            "supportedProtocolVersions",
            Value::Arr(SUPPORTED.iter().map(|v| Value::str(*v)).collect()),
        ),
        ("capabilities", capabilities()),
        ("serverInfo", server_info()),
        ("instructions", Value::str(INSTRUCTIONS)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-mcp-{}-{}-{}",
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
            "# Start\n\nIntro text.\n\n## Install\n\nRun the installer.\n",
        )
        .unwrap();
        d
    }

    /// Runs one request against a fixture tree and returns the reply.
    fn ask(dir: &Path, headers: &[(&str, &str)], body: &str) -> Reply {
        let catalog = Catalog::scan(dir).unwrap();
        let snap = catalog.current();
        let plugins = plugin::Set::default();
        let ctx = Ctx { root: dir, snap: &snap, plugins: &plugins, engine: None };
        let hs: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        handle(body, &hs, &ctx)
    }

    fn legacy(dir: &Path, body: &str) -> Value {
        let r = ask(dir, &[], body);
        json::parse(&r.body).unwrap()
    }

    #[test]
    fn initialize_is_answered_for_legacy_clients() {
        let d = fixture();
        let v = legacy(
            &d,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        let result = v.get("result").unwrap();
        assert_eq!(
            result.get("protocolVersion").unwrap().as_str(),
            Some("2025-06-18"),
            "the client's own version is echoed when supported"
        );
        assert!(result.get("capabilities").unwrap().get("tools").is_some());
        assert_eq!(
            result.get("serverInfo").unwrap().get("name").unwrap().as_str(),
            Some("serve-md")
        );
    }

    #[test]
    fn an_unknown_client_version_is_answered_with_the_newest() {
        let d = fixture();
        let v = legacy(
            &d,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        );
        assert_eq!(
            v.get("result").unwrap().get("protocolVersion").unwrap().as_str(),
            Some(PROTOCOL_LATEST)
        );
    }

    #[test]
    fn modern_discovery_needs_no_handshake() {
        let d = fixture();
        let r = ask(
            &d,
            &[("MCP-Protocol-Version", PROTOCOL_LATEST), ("Mcp-Method", "server/discover")],
            r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
        );
        assert_eq!(r.status, 200);
        let v = json::parse(&r.body).unwrap();
        let result = v.get("result").unwrap();
        assert_eq!(result.get("protocolVersion").unwrap().as_str(), Some(PROTOCOL_LATEST));
        assert!(result.get("instructions").unwrap().as_str().unwrap().contains("search_docs"));
    }

    #[test]
    fn notifications_are_accepted_with_no_body() {
        let d = fixture();
        for body in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
        ] {
            let r = ask(&d, &[], body);
            assert_eq!(r.status, 202, "{body}");
            assert!(r.body.is_empty());
        }
    }

    #[test]
    fn malformed_json_is_a_parse_error_not_a_panic() {
        let d = fixture();
        let r = ask(&d, &[], "{not json");
        assert_eq!(r.status, 400);
        let v = json::parse(&r.body).unwrap();
        assert_eq!(v.get("error").unwrap().get("code").unwrap().as_i64(), Some(PARSE_ERROR));
        assert!(v.get("id").unwrap().is_null());
    }

    #[test]
    fn a_message_without_a_method_is_an_invalid_request() {
        let d = fixture();
        let v = legacy(&d, r#"{"jsonrpc":"2.0","id":3}"#);
        assert_eq!(
            v.get("error").unwrap().get("code").unwrap().as_i64(),
            Some(INVALID_REQUEST)
        );
    }

    #[test]
    fn unknown_methods_differ_by_era() {
        let d = fixture();
        // Legacy: 200, so the client reads the error from the body rather than
        // concluding the endpoint is missing.
        let legacy_reply = ask(&d, &[], r#"{"jsonrpc":"2.0","id":1,"method":"nope/nope"}"#);
        assert_eq!(legacy_reply.status, 200);

        let modern = ask(
            &d,
            &[("MCP-Protocol-Version", PROTOCOL_LATEST), ("Mcp-Method", "nope/nope")],
            r#"{"jsonrpc":"2.0","id":1,"method":"nope/nope"}"#,
        );
        assert_eq!(modern.status, 404);
        let v = json::parse(&modern.body).unwrap();
        assert_eq!(
            v.get("error").unwrap().get("code").unwrap().as_i64(),
            Some(METHOD_NOT_FOUND)
        );
    }

    #[test]
    fn an_unsupported_protocol_version_lists_what_is_supported() {
        let d = fixture();
        let r = ask(
            &d,
            &[("MCP-Protocol-Version", "1999-01-01")],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        assert_eq!(r.status, 400);
        let v = json::parse(&r.body).unwrap();
        let supported = v.get("error").unwrap().get("data").unwrap().get("supported").unwrap();
        assert_eq!(supported.as_arr().unwrap().len(), SUPPORTED.len());
    }

    #[test]
    fn a_header_disagreeing_with_the_body_is_rejected() {
        let d = fixture();
        // Protocol version.
        let r = ask(
            &d,
            &[("MCP-Protocol-Version", "2025-06-18"), ("Mcp-Method", "ping")],
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
        );
        assert_eq!(r.status, 400);
        let v = json::parse(&r.body).unwrap();
        assert_eq!(
            v.get("error").unwrap().get("code").unwrap().as_i64(),
            Some(HEADER_MISMATCH)
        );

        // Method name.
        let r = ask(
            &d,
            &[("Mcp-Method", "tools/list")],
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert_eq!(r.status, 400);
    }

    #[test]
    fn modern_requests_must_carry_the_routing_headers() {
        let d = fixture();
        let missing = ask(
            &d,
            &[("MCP-Protocol-Version", PROTOCOL_LATEST)],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        assert_eq!(missing.status, 400, "Mcp-Method is required of a modern client");

        // The same request from a legacy client is fine without them.
        let legacy_ok = ask(&d, &[], r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        assert_eq!(legacy_ok.status, 200);
    }

    #[test]
    fn a_base64_encoded_name_header_is_decoded_before_comparison() {
        assert_eq!(decode_header_value("plain").as_deref(), Some("plain"));
        // "Hello, 世界"
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert!(decode_header_value("=?base64?!!!not base64!!!?=").is_none());
    }

    #[test]
    fn list_results_carry_cache_hints_only_for_modern_clients() {
        let d = fixture();
        let modern = ask(
            &d,
            &[("MCP-Protocol-Version", PROTOCOL_LATEST), ("Mcp-Method", "tools/list")],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let v = json::parse(&modern.body).unwrap();
        assert!(v.get("result").unwrap().get("ttlMs").is_some());

        let old = legacy(&d, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        assert!(old.get("result").unwrap().get("ttlMs").is_none());
    }

    #[test]
    fn era_detection_is_a_date_comparison() {
        assert!(is_modern("2026-07-28"));
        assert!(is_modern("2027-01-01"));
        assert!(!is_modern("2025-11-25"));
        assert!(!is_modern("2025-03-26"));
    }

    #[test]
    fn ping_is_an_empty_object() {
        let d = fixture();
        let v = legacy(&d, r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#);
        assert_eq!(*v.get("result").unwrap(), Value::Obj(vec![]));
        assert_eq!(v.get("id").unwrap().as_i64(), Some(9));
    }

    #[test]
    fn a_string_id_is_echoed_unchanged() {
        let d = fixture();
        let v = legacy(&d, r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#);
        assert_eq!(v.get("id").unwrap().as_str(), Some("abc"));
    }
}
