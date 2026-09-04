//! The `sitemap` plugin: gates the generated `/sitemap.xml` route.
//!
//! Same shape as `x-headers` (see its module doc): the actual work — building
//! the XML — lives outside the render pipeline entirely, in `crate::sitemap`,
//! and `http::route` asks `plugins.has("sitemap")` to decide whether to
//! answer the route at all. This type exists only so the name shows up in
//! `--plugin`/`--help` and can be resolved and deduplicated like every other
//! plugin.

use super::Plugin;

pub struct Sitemap;

impl Plugin for Sitemap {
    fn name(&self) -> &'static str {
        "sitemap"
    }

    fn describe(&self) -> &'static str {
        "serve a generated /sitemap.xml listing every document"
    }

    // configure/transform/head keep their defaults: this plugin touches no
    // Markdown and contributes no markup, only a route.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Set;

    #[test]
    fn the_plugin_is_registered_and_described() {
        assert_eq!(Sitemap.name(), "sitemap");
        assert!(Sitemap.describe().contains("sitemap.xml"));
        assert!(crate::plugin::catalog().iter().any(|l| l.starts_with("sitemap — ")));
        assert!(Set::resolve(&["sitemap".to_string()]).unwrap().has("sitemap"));
    }

    #[test]
    fn the_plugin_changes_no_markup() {
        let with = Set::resolve(&["sitemap".to_string()]).unwrap();
        let without = Set::default();
        let src = "# Title\n\nSome prose.\n";
        assert_eq!(with.render_html(src).html, without.render_html(src).html);
        assert!(with.render_html(src).head.is_empty());
    }
}
