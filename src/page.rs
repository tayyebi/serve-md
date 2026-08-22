use crate::encoding::percent_encode_path;
use crate::scanner::FileEntry;
use crate::template::Template;
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn render_markdown(src: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.strikethrough = true;
    options.extension.tagfilter = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    comrak::markdown_to_html(src, &options)
}

pub fn listing_html(files: &[FileEntry], dir: &Path) -> String {
    let dir_str = dir.display().to_string();
    let body = body_html(files, &dir_str);
    render_page("Files", &body, files.len(), &dir_str)
}

pub fn view_html(rel: &str, markdown_html: &str, dir: &Path, file_count: usize) -> String {
    let dir_str = dir.display().to_string();
    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("name", rel);
    vars.insert("content", markdown_html);
    let body = Template::new(include_str!("../templates/view.html")).render(&vars, &[], &[]);
    render_page(rel, &body, file_count, &dir_str)
}

pub fn listing_plain(files: &[FileEntry], dir: &Path) -> String {
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
    out.push_str("\nview a file:        curl <base>/<path>\n");
    out.push_str("force markdown/text: curl -H 'Accept: text/markdown' <base>/<path>\n");
    out
}

pub fn not_found_html() -> String {
    render_page(
        "Not found",
        include_str!("../templates/not_found.html"),
        0,
        ".",
    )
}

pub fn unauthorized_html() -> String {
    render_page(
        "Unauthorized",
        include_str!("../templates/unauthorized.html"),
        0,
        ".",
    )
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

fn body_html(files: &[FileEntry], dir: &str) -> String {
    let tpl = Template::new(include_str!("../templates/listing.html"));
    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("dir", dir);
    let flags: Vec<&str> = if files.is_empty() {
        vec!["empty"]
    } else {
        vec!["nonempty"]
    };
    let mut rows: Vec<Vec<(String, String)>> = Vec::with_capacity(files.len());
    for f in files {
        rows.push(vec![
            ("link".to_string(), percent_encode_path(&f.rel)),
            ("name".to_string(), f.rel.clone()),
            ("size".to_string(), format_size(f.size)),
            ("mtime".to_string(), format_time(f.modified)),
        ]);
    }
    tpl.render(&vars, &rows, &flags)
}

fn render_page(title: &str, body: &str, file_count: usize, dir: &str) -> String {
    let count = file_count.to_string();
    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("title", title);
    vars.insert("version", env!("CARGO_PKG_VERSION"));
    vars.insert("file_count", &count);
    vars.insert("dir", dir);
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
    fn time_formatting() {
        let t = SystemTime::UNIX_EPOCH;
        assert_eq!(format_time(t), "1970-01-01 00:00 UTC");
        let later = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(format_time(later), "2023-11-14 22:13 UTC");
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
        let out = listing_plain(&files, Path::new("/tmp/x"));
        assert!(out.starts_with("serve-md "));
        assert!(out.contains("a.md"));
        assert!(out.contains("docs/guide.md"));
        assert!(out.contains("2.0 KiB"));
        assert!(out.contains("curl <base>/<path>"));
    }

    #[test]
    fn listing_html_escapes_names() {
        let files = vec![FileEntry {
            rel: "a<b>.md".into(),
            size: 1,
            modified: SystemTime::UNIX_EPOCH,
        }];
        let out = listing_html(&files, Path::new("/tmp"));
        assert!(out.contains("a&lt;b&gt;.md"));
        assert!(!out.contains("a<b>.md"));
    }

    #[test]
    fn view_html_contains_content() {
        let out = view_html("docs/my file.md", "<p>hi</p>", Path::new("/tmp"), 3);
        assert!(out.contains("my file.md"));
        assert!(out.contains("<p>hi</p>"));
    }
}
