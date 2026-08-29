//! The served file list, shared by every surface that needs one.
//!
//! Before this module the list lived in `http::Ctx` as a plain `Vec`, scanned
//! once at startup and never refreshed — so a document added while the server
//! ran was missing from the listing until a restart. Three surfaces now read
//! it (the listing page, the MCP tools, and the generated `llms.txt`), and a
//! stale answer to an agent is worse than a stale web page: an agent will
//! state as fact that a document does not exist.
//!
//! So the list lives behind one `Catalog`. Readers call [`Catalog::current`]
//! and get an `Arc<Snapshot>`; they never block on a scan and never see a
//! half-built list. Under `--fresh` a background thread re-walks the tree and
//! swaps a new snapshot in.
//!
//! # Why polling rather than filesystem events
//!
//! Native change notification means FSEvents on macOS, inotify on Linux and
//! `ReadDirectoryChangesW` on Windows. All three are C APIs reachable only
//! through FFI, which would mean either the `libc`/`notify` crates or
//! hand-written `extern` blocks. Both end the single-dependency property, and
//! the release workflow's own comment warns that the no-musl-toolchain
//! shortcut "holds only while the tree stays pure Rust — a dependency that
//! compiles C would need musl-gcc".
//!
//! A polling walk costs a `readdir` and a `stat` per entry, on a tree already
//! small enough to serve from memory, once a second. That buys identical
//! behaviour on all three release targets for about sixty lines and no
//! dependency.

use crate::scanner::{scan, FileEntry};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

/// One consistent view of the served tree.
#[derive(Debug, Default)]
pub struct Snapshot {
    /// Sorted by `rel`, as [`scan`] returns it — which is what lets
    /// [`Snapshot::get`] binary-search.
    pub files: Vec<FileEntry>,
}

impl Snapshot {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// The entry for a root-relative path, or `None` if the tree does not
    /// serve it.
    ///
    /// This is the membership test the search layer relies on: a path absent
    /// here is a path the listing would not show, and so a path no answer may
    /// mention. See [`crate::search::filter_to_catalog`].
    pub fn get(&self, rel: &str) -> Option<&FileEntry> {
        self.files
            .binary_search_by(|f| f.rel.as_str().cmp(rel))
            .ok()
            .map(|i| &self.files[i])
    }

    pub fn contains(&self, rel: &str) -> bool {
        self.get(rel).is_some()
    }

    /// Whether two snapshots describe the same tree.
    ///
    /// Compares path, size and mtime of every entry — enough to notice a file
    /// added, removed, renamed or edited, without reading a byte of content.
    fn same_as(&self, other: &Snapshot) -> bool {
        self.files.len() == other.files.len()
            && self.files.iter().zip(&other.files).all(|(a, b)| {
                a.rel == b.rel && a.size == b.size && a.modified == b.modified
            })
    }
}

pub struct Catalog {
    root: PathBuf,
    snap: RwLock<Arc<Snapshot>>,
}

impl Catalog {
    /// Walks the tree once. Fails the same way startup already failed on an
    /// unreadable `--dir`, so a typo is still reported before the port binds.
    pub fn scan(root: &Path) -> io::Result<Catalog> {
        let files = scan(root)?;
        Ok(Catalog {
            root: root.to_path_buf(),
            snap: RwLock::new(Arc::new(Snapshot { files })),
        })
    }

    /// The current view. Cheap — it clones an `Arc` and touches no disk — so
    /// callers should take one per request and use it throughout, rather than
    /// calling repeatedly and risking two halves of one response disagreeing.
    pub fn current(&self) -> Arc<Snapshot> {
        // A panic in a writer must not take the server down with it: the worst
        // a poisoned lock can hold here is a file list.
        self.snap
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Re-walks the tree, swapping the snapshot in only if something moved.
    /// Returns whether it did.
    ///
    /// The scan happens outside the write lock, so readers are blocked only
    /// for the pointer swap.
    pub fn refresh(&self) -> io::Result<bool> {
        let fresh = Snapshot { files: scan(&self.root)? };
        if self.current().same_as(&fresh) {
            return Ok(false);
        }
        let mut guard = self.snap.write().unwrap_or_else(|e| e.into_inner());
        *guard = Arc::new(fresh);
        Ok(true)
    }

    /// Starts the `--fresh` watcher: a detached thread that re-walks the tree
    /// every `interval` and swaps in anything new.
    ///
    /// Scan errors are ignored rather than fatal. A transient failure —  the
    /// directory being replaced by an editor's atomic rename, say — must not
    /// end the watcher and silently freeze the catalog for the rest of the
    /// process's life; the next tick simply tries again.
    pub fn spawn_watcher(self: &Arc<Self>, interval: Duration, verbose: bool) {
        let catalog = Arc::clone(self);
        thread::spawn(move || loop {
            thread::sleep(interval);
            let changed = catalog.refresh().unwrap_or(false);
            if changed && verbose {
                println!("[verbose] catalog: {} file(s)", catalog.current().len());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "serve-md-catalog-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn lookup_finds_served_paths_only() {
        let d = tmp("get");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.md"), "a").unwrap();
        fs::write(d.join("sub/b.md"), "bb").unwrap();

        let snap = Catalog::scan(&d).unwrap().current();
        assert!(snap.contains("a.md"));
        assert!(snap.contains("sub/b.md"));
        assert_eq!(snap.get("sub/b.md").unwrap().size, 2);
        assert!(!snap.contains("missing.md"));
        // The scanner never lists these, so the catalog must not know them.
        assert!(!snap.contains(".git/config"));
        assert!(!snap.contains("../outside.md"));
    }

    #[test]
    fn refresh_reports_only_real_changes() {
        let d = tmp("refresh");
        fs::write(d.join("a.md"), "a").unwrap();
        let cat = Catalog::scan(&d).unwrap();
        assert_eq!(cat.current().len(), 1);

        assert!(!cat.refresh().unwrap(), "nothing moved");

        fs::write(d.join("b.md"), "b").unwrap();
        assert!(cat.refresh().unwrap(), "a file appeared");
        assert_eq!(cat.current().len(), 2);
        assert!(cat.current().contains("b.md"));

        fs::remove_file(d.join("b.md")).unwrap();
        assert!(cat.refresh().unwrap(), "a file vanished");
        assert_eq!(cat.current().len(), 1);
    }

    #[test]
    fn refresh_notices_an_edit_that_changes_size() {
        let d = tmp("edit");
        fs::write(d.join("a.md"), "short").unwrap();
        let cat = Catalog::scan(&d).unwrap();
        fs::write(d.join("a.md"), "much longer than before").unwrap();
        assert!(cat.refresh().unwrap());
        assert_eq!(cat.current().get("a.md").unwrap().size, 23);
    }

    #[test]
    fn a_held_snapshot_is_unaffected_by_a_refresh() {
        // The point of handing out an `Arc`: a request that took its view at
        // the start still sees a consistent tree at the end.
        let d = tmp("hold");
        fs::write(d.join("a.md"), "a").unwrap();
        let cat = Catalog::scan(&d).unwrap();
        let held = cat.current();

        fs::write(d.join("b.md"), "b").unwrap();
        cat.refresh().unwrap();

        assert_eq!(held.len(), 1, "the held snapshot did not change underneath");
        assert_eq!(cat.current().len(), 2);
    }

    #[test]
    fn watcher_picks_up_a_new_file() {
        let d = tmp("watch");
        fs::write(d.join("a.md"), "a").unwrap();
        let cat = Arc::new(Catalog::scan(&d).unwrap());
        cat.spawn_watcher(Duration::from_millis(20), false);

        fs::write(d.join("b.md"), "b").unwrap();
        // Poll rather than sleeping a fixed span, so the test is not a race on
        // a slow machine.
        let mut seen = false;
        for _ in 0..100 {
            if cat.current().contains("b.md") {
                seen = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(seen, "the watcher never noticed b.md");
    }
}
