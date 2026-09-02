use crate::encoding::percent_encode_path;
use crate::scanner::FileEntry;
use crate::template::Template;
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// `head` is extra `<head>` markup contributed by render plugins.
///
/// Passed in rather than derived here because the listing renders no Markdown
/// of its own: a plugin that contributes markup to every page — `webmcp` —
/// would otherwise miss the page most visitors see first.
pub fn listing_html(files: &[FileEntry], head: &str) -> String {
    render_page("Files", &body_html(files), head)
}

/// `head` is extra `<head>` markup contributed by render plugins; empty for
/// documents no plugin touched.
///
/// `rel` is the `<title>` only. The page itself is the document and nothing
/// else — no breadcrumb, no name repeated above the content.
pub fn view_html(rel: &str, markdown_html: &str, head: &str) -> String {
    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("content", markdown_html);
    let body = Template::new(include_str!("../templates/view.html")).render(&vars, &[], &[]);
    render_page(rel, &body, head)
}

/// The listing a terminal client sees. `mcp` says whether to mention the
/// agent-facing endpoints, which exist only under `--plugin webmcp`.
pub fn listing_plain(files: &[FileEntry], dir: &Path, mcp: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "serve-md {} · {} file(s) in {}\n",
        env!("CARGO_PKG_VERSION"),
        files.len(),
        dir.display()
    ));
    if files.is_empty() {
        out.push_str("(no files found)\n");
        return out;
    }
    let width = files.iter().map(|f| f.rel.len()).max().unwrap_or(0);
    for f in files {
        out.push_str(&f.rel);
        for _ in f.rel.len()..width {
            out.push(' ');
        }
        out.push_str("  ");
        out.push_str(&format!("{:>9}", format_size(f.size)));
        out.push_str("  ");
        out.push_str(&format_time(f.modified));
        out.push('\n');
    }
    out.push_str("\nview a file:         curl <base>/<path>\n");
    out.push_str("force markdown/text: curl -H 'Accept: text/markdown' <base>/<path>\n");
    if mcp {
        out.push_str("index for models:    curl <base>/llms.txt\n");
        out.push_str("mcp endpoint:        POST <base>/mcp\n");
    }
    out
}

pub fn not_found_html() -> String {
    render_page("Not found", include_str!("../templates/not_found.html"), "")
}

pub fn unauthorized_html() -> String {
    render_page("Unauthorized", include_str!("../templates/unauthorized.html"), "")
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn format_time(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let minute = rem / 60 % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02} UTC")
}

/// The `Last-Modified` / `If-Modified-Since` format: IMF-fixdate, RFC 9110
/// §5.6.7. Deliberately not [`format_time`] — that one is for humans reading
/// the listing, and its `YYYY-MM-DD HH:MM UTC` is not a date any HTTP client
/// will parse.
///
/// Fixed English day and month abbreviations, and `GMT` spelled literally, are
/// what the grammar requires; a locale-aware formatter would be a bug here.
pub fn format_http_date(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday, index 4 in a week starting at Sunday.
    let wd = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    let mo = MONTHS[(m - 1) as usize];
    let (h, mi, se) = (rem / 3600, rem / 60 % 60, rem % 60);
    format!("{wd}, {d:02} {mo} {y:04} {h:02}:{mi:02}:{se:02} GMT")
}

/// Parses an IMF-fixdate back to seconds since the epoch.
///
/// Only that one format, though RFC 9110 §5.6.7 also lists two obsolete ones.
/// The single caller is conditional-request handling, where an unparsed date
/// means the full response is served — correct, merely wasteful — so the cost
/// of not accepting a form no client has sent since the 1990s is nothing.
pub fn parse_http_date(s: &str) -> Option<u64> {
    // Sun, 06 Nov 1994 08:49:37 GMT
    let s = s.trim();
    let rest = s.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let d: u32 = parts.next()?.parse().ok()?;
    let mon = parts.next()?;
    let y: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    let m = MONTHS.iter().position(|x| *x == mon)? as u32 + 1;
    if d == 0 || d > 31 {
        return None;
    }
    let mut hms = time.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let se: i64 = hms.next()?.parse().ok()?;
    if hms.next().is_some() || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The inverse of [`civil_from_days`], same algorithm read backwards.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// The file table.
///
/// It does not name the directory being served. The path is the host's
/// filesystem layout, it means nothing to a visitor, and on a public deploy
/// printing it on every page is a disclosure for no benefit.
fn body_html(files: &[FileEntry]) -> String {
    let tpl = Template::new(include_str!("../templates/listing.html"));
    let vars: HashMap<&str, &str> = HashMap::new();
    let flags: Vec<&str> = if files.is_empty() {
        vec!["empty"]
    } else {
        vec!["nonempty"]
    };
    let mut sorted = files.to_vec();
    sorted.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.rel.cmp(&b.rel)));
    let mut rows: Vec<Vec<(String, String)>> = Vec::with_capacity(sorted.len());
    for f in &sorted {
        let mtime_secs = f
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        rows.push(vec![
            ("link".to_string(), percent_encode_path(&f.rel)),
            ("name".to_string(), f.rel.clone()),
            ("size".to_string(), format_size(f.size)),
            ("mtime".to_string(), format_time(f.modified)),
            ("sort_name".to_string(), f.rel.clone()),
            ("sort_size".to_string(), f.size.to_string()),
            ("sort_mtime".to_string(), mtime_secs),
        ]);
    }
    tpl.render(&vars, &rows, &flags)
}

/// Wraps `body` in the document shell: a `<title>`, whatever `<head>` markup
/// the plugins asked for, and `<main>`.
///
/// Deliberately nothing else. No banner, no footer, no version string, no
/// "serving <path>" — the served document is the page. Chrome would be the
/// server talking over the author, and the footer's `serving {dir}` also put
/// a filesystem path from the host onto every page it rendered.
fn render_page(title: &str, body: &str, head: &str) -> String {
    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("title", title);
    // Always inserted, even when empty: an unmatched {{...}} would be left in
    // the output as literal text.
    vars.insert("head", head);
    vars.insert("body", body);
    Template::new(include_str!("../templates/base.html")).render(&vars, &[], &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(2048), "2.0 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn http_date_formatting() {
        let at = |s| UNIX_EPOCH + std::time::Duration::from_secs(s);
        assert_eq!(
            format_http_date(SystemTime::UNIX_EPOCH),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
        // The example date from RFC 9110 §5.6.7 itself.
        assert_eq!(format_http_date(at(784_111_777)), "Sun, 06 Nov 1994 08:49:37 GMT");
        // A leap day, which is where a hand-written calendar goes wrong.
        assert_eq!(format_http_date(at(1_709_164_800)), "Thu, 29 Feb 2024 00:00:00 GMT");
    }

    #[test]
    fn http_date_round_trips() {
        for secs in [0u64, 784_111_777, 1_709_164_800, 1_700_000_000, 4_102_444_800] {
            let t = UNIX_EPOCH + std::time::Duration::from_secs(secs);
            assert_eq!(parse_http_date(&format_http_date(t)), Some(secs));
        }
    }

    #[test]
    fn a_date_that_cannot_be_parsed_is_none() {
        // Every one of these makes the caller serve the full response, which
        // is the safe direction.
        assert_eq!(parse_http_date(""), None);
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 UTC"), None);
        assert_eq!(parse_http_date("Sun, 06 Xxx 1994 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 25:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 40 Nov 1994 08:49:37 GMT"), None);
        // Before the epoch: no u64 to return.
        assert_eq!(parse_http_date("Mon, 01 Jan 1900 00:00:00 GMT"), None);
    }

    #[test]
    fn the_weekday_is_not_taken_on_trust() {
        // A client may send any weekday it likes; parsing ignores it, so a
        // wrong one must not shift the instant.
        assert_eq!(
            parse_http_date("Mon, 06 Nov 1994 08:49:37 GMT"),
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT")
        );
    }

    #[test]
    fn time_formatting() {
        let t = SystemTime::UNIX_EPOCH;
        assert_eq!(format_time(t), "1970-01-01 00:00 UTC");
        let later = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(format_time(later), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn listing_plain_mentions_the_agent_routes_only_when_enabled() {
        let files = vec![FileEntry {
            rel: "a.md".into(),
            size: 1,
            modified: SystemTime::UNIX_EPOCH,
        }];
        let off = listing_plain(&files, Path::new("/tmp"), false);
        assert!(!off.contains("/mcp"));
        let on = listing_plain(&files, Path::new("/tmp"), true);
        assert!(on.contains("POST <base>/mcp"));
        assert!(on.contains("/llms.txt"));
    }

    #[test]
    fn listing_html_carries_plugin_head_markup() {
        let out = listing_html(&[], "<script>x()</script>");
        assert!(out.contains("<script>x()</script>"));
    }

    #[test]
    fn listing_plain_is_terminal_friendly() {
        let mut files = vec![
            FileEntry {
                rel: "docs/guide.md".into(),
                size: 2048,
                modified: SystemTime::UNIX_EPOCH,
            },
            FileEntry {
                rel: "a.md".into(),
                size: 10,
                modified: SystemTime::UNIX_EPOCH,
            },
        ];
        files.sort_by_cached_key(|f| f.rel.clone());
        let out = listing_plain(&files, Path::new("/tmp/x"), false);
        assert!(out.starts_with("serve-md "));
        assert!(out.contains("a.md"));
        assert!(out.contains("docs/guide.md"));
        assert!(out.contains("2.0 KiB"));
        assert!(out.contains("curl <base>/<path>"));
    }

    #[test]
    fn listing_html_does_not_disclose_the_served_path() {
        let out = listing_html(&[], "");
        assert!(!out.contains("Serving"));
        assert!(!out.contains("/tmp"));
    }

    #[test]
    fn listing_html_escapes_names() {
        let files = vec![FileEntry {
            rel: "a<b>.md".into(),
            size: 1,
            modified: SystemTime::UNIX_EPOCH,
        }];
        let out = listing_html(&files, "");
        assert!(out.contains("a&lt;b&gt;.md"));
        assert!(!out.contains("a<b>.md"));
        // Sort keys ride along for the client-side table sort.
        assert!(out.contains(r#"data-col="size" data-value="1""#));
        assert!(out.contains(r#"data-col="mtime" data-value="0""#));
    }

    #[test]
    fn listing_html_sorts_by_modified_descending_by_default() {
        let old = SystemTime::UNIX_EPOCH;
        let new = old + std::time::Duration::from_secs(1000);
        let files = vec![
            FileEntry {
                rel: "old.md".into(),
                size: 10,
                modified: old,
            },
            FileEntry {
                rel: "newer.md".into(),
                size: 20,
                modified: new,
            },
        ];
        let out = listing_html(&files, "");
        let old_pos = out.find("old.md").unwrap();
        let new_pos = out.find("newer.md").unwrap();
        assert!(new_pos < old_pos, "newest file should come first");
    }

    #[test]
    fn view_html_contains_content() {
        let out = view_html("docs/my file.md", "<p>hi</p>", "");
        assert!(out.contains("<p>hi</p>"));
        // The path is the tab's title, not something printed onto the page.
        assert!(out.contains("<title>docs/my file.md</title>"));
    }

    #[test]
    fn view_html_injects_plugin_head_markup_unescaped() {
        let out = view_html("a.md", "<p>hi</p>", "<style>math{}</style>");
        assert!(out.contains("<style>math{}</style>"));
        assert!(!out.contains("&lt;style&gt;"));
    }

    #[test]
    fn pages_without_plugins_have_no_head_markup() {
        let out = listing_html(&[], "");
        assert!(!out.contains("<style"));
    }
}
