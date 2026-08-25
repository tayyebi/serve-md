//! Optional render extensions for the Markdown pipeline.
//!
//! Plugins are compiled into the binary and listed in [`REGISTRY`]. None run
//! unless the user opts in with `--plugin <NAME>`, so the default output is
//! plain CommonMark exactly as it was before this module existed.
//!
//! Adding a plugin means writing a module that implements [`Plugin`] and adding
//! one line to [`REGISTRY`].

use comrak::nodes::AstNode;
use comrak::{parse_document, Arena, Options};

mod math;
mod mermaid;

/// A render extension. Hooks the pipeline at three points: parser setup, an AST
/// pass, and `<head>` markup.
pub trait Plugin: Sync {
    /// The name used by `--plugin <NAME>`.
    fn name(&self) -> &'static str;

    /// One-line summary shown in `--help`.
    fn describe(&self) -> &'static str;

    /// Turns on the parser extensions this plugin needs.
    ///
    /// This applies to *every* output format, so the terminal renderer sees the
    /// same AST the browser does.
    fn configure(&self, _options: &mut Options<'_>) {}

    /// Rewrites the AST before HTML rendering, returning whether anything
    /// changed — which is what gates [`Plugin::head`].
    ///
    /// HTML output only; the ASCII renderer walks the untransformed AST so that
    /// terminal clients still see the original source constructs. `arena` is
    /// unused by the current plugins but lets a future one allocate new nodes
    /// rather than only rewriting existing ones.
    fn transform<'a>(&self, _arena: &'a Arena<'a>, _root: &'a AstNode<'a>) -> bool {
        false
    }

    /// Markup appended to `<head>`, emitted only when [`Plugin::transform`]
    /// reported a change — so a plugin costs nothing on pages it did not touch.
    fn head(&self) -> Option<&'static str> {
        None
    }
}

static MATH: math::Math = math::Math;
static MERMAID: mermaid::Mermaid = mermaid::Mermaid;

/// Every plugin compiled into this binary.
pub static REGISTRY: &[&dyn Plugin] = &[&MATH, &MERMAID];

/// `name — description` for each registered plugin, for `--help`.
pub fn catalog() -> Vec<String> {
    REGISTRY
        .iter()
        .map(|p| format!("{} — {}", p.name(), p.describe()))
        .collect()
}

/// Comma-separated plugin names, for error messages.
fn available() -> String {
    REGISTRY
        .iter()
        .map(|p| p.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rendered HTML plus any `<head>` markup the plugins contributed.
pub struct Rendered {
    pub html: String,
    pub head: String,
}

/// The plugins enabled for this process. Empty unless `--plugin` was passed.
#[derive(Default)]
pub struct Set(Vec<&'static dyn Plugin>);

/// `&dyn Plugin` cannot derive `Debug`, and the useful view is the names.
impl std::fmt::Debug for Set {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl Set {
    /// Looks each name up in [`REGISTRY`], ignoring repeats. Fails on an
    /// unknown name so a typo is reported at startup rather than silently
    /// serving unrendered pages.
    pub fn resolve(names: &[String]) -> Result<Self, String> {
        let mut active: Vec<&'static dyn Plugin> = Vec::new();
        for name in names {
            let found = REGISTRY.iter().find(|p| p.name() == name.as_str());
            let Some(plugin) = found else {
                return Err(format!(
                    "unknown plugin: {name} (available: {})",
                    available()
                ));
            };
            if !active.iter().any(|a| a.name() == plugin.name()) {
                active.push(*plugin);
            }
        }
        Ok(Self(active))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The names of the active plugins, in the order they were requested.
    pub fn names(&self) -> Vec<&'static str> {
        self.0.iter().map(|p| p.name()).collect()
    }

    /// The single source of truth for comrak parser options.
    ///
    /// Built per call rather than cached: `comrak::Options` holds
    /// `Arc<dyn URLRewriter>` and so is neither `Send` nor `Sync`, which rules
    /// out storing it in the `Arc<Ctx>` shared across connection threads.
    pub fn options(&self) -> Options<'static> {
        let mut options = Options::default();
        options.extension.strikethrough = true;
        options.extension.tagfilter = true;
        options.extension.table = true;
        options.extension.autolink = true;
        options.extension.tasklist = true;
        for plugin in &self.0 {
            plugin.configure(&mut options);
        }
        options
    }

    /// Parses `src`, lets each plugin rewrite the AST, then renders HTML.
    ///
    /// With an empty set this is equivalent to `comrak::markdown_to_html`.
    pub fn render_html(&self, src: &str) -> Rendered {
        let arena = Arena::new();
        let options = self.options();
        let root = parse_document(&arena, src, &options);

        let mut head = String::new();
        for plugin in &self.0 {
            if plugin.transform(&arena, root) {
                head.push_str(plugin.head().unwrap_or_default());
            }
        }

        let mut html = String::new();
        comrak::format_html(root, &options, &mut html)
            .expect("formatting into a String cannot fail");
        Rendered { html, head }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> Set {
        let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        Set::resolve(&owned).unwrap()
    }

    #[test]
    fn empty_by_default() {
        let s = Set::default();
        assert!(s.is_empty());
        assert!(s.names().is_empty());
    }

    #[test]
    fn resolves_known_names_and_dedupes() {
        let s = set(&["math", "math"]);
        assert_eq!(s.names(), vec!["math"]);
        assert_eq!(set(&["mermaid", "math"]).names(), vec!["mermaid", "math"]);
    }

    #[test]
    fn plugins_are_independent() {
        // Enabling one must not turn the other on.
        let only_math = set(&["math"]);
        assert!(!only_math
            .render_html("```mermaid\nflowchart LR\nA-->B\n```\n")
            .html
            .contains("<svg"));
        let only_mermaid = set(&["mermaid"]);
        assert!(!only_mermaid.render_html("$E = mc^2$\n").html.contains("<math"));
    }

    #[test]
    fn rejects_unknown_names() {
        let err = Set::resolve(&["nope".to_string()]).unwrap_err();
        assert!(err.contains("unknown plugin: nope"));
        assert!(err.contains("math"));
    }

    #[test]
    fn empty_set_matches_plain_comrak() {
        let src = "# Title\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n~~gone~~ and $x^2$ stays.\n";
        let s = Set::default();
        let expected = comrak::markdown_to_html(src, &s.options());
        assert_eq!(s.render_html(src).html, expected);
    }

    #[test]
    fn head_is_emitted_only_when_a_plugin_fires() {
        let s = set(&["math"]);
        assert!(s.render_html("just prose, no formulas\n").head.is_empty());
        assert!(!s.render_html("energy is $E = mc^2$ today\n").head.is_empty());
    }

    #[test]
    fn catalog_describes_every_plugin() {
        assert_eq!(catalog().len(), REGISTRY.len());
        assert!(catalog().iter().any(|line| line.starts_with("math — ")));
        assert!(catalog().iter().any(|line| line.starts_with("mermaid — ")));
    }
}
