use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Markdown,
    Html,
    /// Any other file, served as a static asset with a guessed MIME type.
    Static,
}

impl FileKind {
    pub fn from_ext(ext: &str) -> FileKind {
        match ext.to_ascii_lowercase().as_str() {
            "md" | "markdown" => FileKind::Markdown,
            "html" | "htm" => FileKind::Html,
            _ => FileKind::Static,
        }
    }

    pub fn from_path(path: &Path) -> FileKind {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => FileKind::from_ext(ext),
            None => FileKind::Static,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub rel: String,
    pub size: u64,
    pub modified: SystemTime,
}

/// Directory names never listed and never served: version-control metadata
/// and build output, which routinely hold credentials, tokens and the full
/// history of a project.
///
/// Public because `search` passes the same list to the external search tool as
/// exclusions. That is a performance measure and a second line of defence, not
/// the guarantee — hits are still checked against the catalog — but the two
/// must at least name the same directories.
pub const SKIP_DIRS: &[&str] = &[".git", ".hg", ".svn", "target", "node_modules"];

/// The one hidden path a static server is expected to hand out.
const HIDDEN_ALLOWED: &[&str] = &[".well-known"];

/// Whether a single path segment is off limits: a skipped directory, or any
/// hidden (dot-prefixed) name outside the `.well-known` exception — `.env`,
/// `.htpasswd`, `.npmrc` and friends.
///
/// The scanner and the router share this one rule on purpose. They used to
/// disagree: the listing hid `.git`, but a request for `/.git/config` was
/// still served. A name the listing will not show must not be reachable by
/// typing its path either.
pub fn is_forbidden_segment(name: &str) -> bool {
    if HIDDEN_ALLOWED.contains(&name) {
        return false;
    }
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

pub fn scan(root: &Path) -> io::Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by_cached_key(|f| f.rel.clone());
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<FileEntry>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        // Not followed: `file_type` here does not resolve symlinks, so a link
        // is neither a file nor a directory and falls out of the walk.
        let file_type = entry.file_type()?;
        if is_forbidden_segment(&entry.file_name().to_string_lossy()) {
            continue;
        }
        if file_type.is_dir() {
            walk(root, &entry.path(), out)?;
        } else if file_type.is_file() {
            let meta = entry.metadata()?;
            let rel = rel_str(root, &entry.path());
            out.push(FileEntry {
                rel,
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
    Ok(())
}

fn rel_str(root: &Path, full: &Path) -> String {
    let rel = full.strip_prefix(root).unwrap_or(full);
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&comp.as_os_str().to_string_lossy());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-scan-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn recursive_sorted_and_skips_dotdirs() {
        let d = tmp("t");
        fs::create_dir_all(d.join("sub/deep")).unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::create_dir_all(d.join("target")).unwrap();
        fs::write(d.join("b.md"), "b").unwrap();
        fs::write(d.join("a.md"), "a").unwrap();
        fs::write(d.join("sub/c.md"), "c").unwrap();
        fs::write(d.join("sub/deep/d.md"), "d").unwrap();
        fs::write(d.join(".git/x.md"), "x").unwrap();
        fs::write(d.join("target/y.md"), "y").unwrap();
        fs::write(d.join("note.txt"), "n").unwrap();
        fs::write(d.join("README.MD"), "r").unwrap();

        let files = scan(&d).unwrap();
        let rels: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
        assert_eq!(
            rels,
            vec!["README.MD", "a.md", "b.md", "note.txt", "sub/c.md", "sub/deep/d.md"]
        );
    }

    #[test]
    fn scans_html_and_static_files_too() {
        let d = tmp("html");
        fs::write(d.join("page.html"), "<h1>hi</h1>").unwrap();
        fs::write(d.join("frag.htm"), "<p>hi</p>").unwrap();
        fs::write(d.join("doc.md"), "# hi").unwrap();
        fs::write(d.join("style.css"), "body{}").unwrap();
        fs::write(d.join("favicon.ico"), "x").unwrap();

        let files = scan(&d).unwrap();
        let rels: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
        assert_eq!(
            rels,
            vec!["doc.md", "favicon.ico", "frag.htm", "page.html", "style.css"]
        );
    }

    #[test]
    fn classifies_by_extension() {
        assert_eq!(FileKind::from_ext("md"), FileKind::Markdown);
        assert_eq!(FileKind::from_ext("MARKDOWN"), FileKind::Markdown);
        assert_eq!(FileKind::from_ext("html"), FileKind::Html);
        assert_eq!(FileKind::from_ext("HTM"), FileKind::Html);
        assert_eq!(FileKind::from_ext("css"), FileKind::Static);
        assert_eq!(FileKind::from_path(Path::new("page.html")), FileKind::Html);
        assert_eq!(FileKind::from_path(Path::new("noext")), FileKind::Static);
    }

    #[test]
    fn empty_dir() {
        let d = tmp("empty");
        let files = scan(&d).unwrap();
        assert!(files.is_empty());
    }
}
