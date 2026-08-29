//! The `webmcp` plugin: agent access to the served documents.
//!
//! Enabling it does two things.
//!
//! **On the server**, it turns on the `POST /mcp` endpoint (see [`crate::mcp`])
//! and the generated `/llms.txt`. `http::route` gates both on
//! `plugins.has("webmcp")`, so a serve-md started without the plugin is
//! byte-for-byte the server it was before this module existed.
//!
//! **In the browser**, it injects the script below, which registers the same
//! tools with the page's own agent through the W3C WebMCP API. A page that
//! does this is, in the words of the explainer, an MCP server whose tools are
//! implemented in client-side script — so a browser-resident agent can search
//! these documents with no configuration at all, no server URL to paste, and
//! no credentials to hand over.
//!
//! # Why the script fetches instead of implementing anything
//!
//! The registered tools do nothing except `POST /mcp`. The alternative —
//! reimplementing search and outline extraction in JavaScript — would mean two
//! implementations of every tool, which would differ, and the browser copy
//! would be the one nobody tested. Here the browser surface is a thin
//! forwarder, and the tool list itself is fetched from `tools/list` at load,
//! so tools added to the server appear in the browser without touching this
//! file.
//!
//! # Why this plugin turns on heading ids
//!
//! [`Plugin::configure`] sets `header_id_prefix`, which makes comrak emit
//! `id` attributes on headings. `search_docs` and `get_outline` answer with
//! `/path#anchor` links, and without ids on the page those links would scroll
//! nowhere. The anchors themselves come from `comrak::Anchorizer`, the same
//! type comrak uses to generate the ids, so the two cannot drift apart.
//!
//! # References
//!
//! - WebMCP explainer, W3C Web Machine Learning CG:
//!   <https://github.com/webmachinelearning/webmcp>
//! - `document.modelContext.registerTool()`, Community Group Draft Report,
//!   April 2026. `provideContext()`/`clearContext()` were removed in the March
//!   2026 revision; `registerTool`/`unregisterTool` are the only way to
//!   declare tools. Shipped in Chrome 146 and Edge 147, where it was reached
//!   as `navigator.modelContext` — which the script still falls back to.

use super::Plugin;
use comrak::Options;

pub struct WebMcp;

impl Plugin for WebMcp {
    fn name(&self) -> &'static str {
        "webmcp"
    }

    fn describe(&self) -> &'static str {
        "expose the documents to AI agents over MCP (/mcp, /llms.txt, and in-browser WebMCP)"
    }

    /// Turns on heading ids, so the `#anchor` links the MCP tools hand out
    /// resolve on the rendered page.
    fn configure(&self, options: &mut Options<'_>) {
        // An empty prefix means the id is the bare slug — `id="install"`, so
        // `/guides/start.md#install` works — rather than GitHub's
        // `user-content-` namespacing, which nothing here needs.
        options.extension.header_id_prefix = Some(String::new());
    }

    /// Always true, unlike `math` and `mermaid`, which report whether they
    /// changed anything.
    ///
    /// [`Plugin::head`] is gated on this return value, and this plugin's
    /// markup belongs on every page: a document with no formulas needs no
    /// MathML stylesheet, but every page should tell a visiting agent what it
    /// can do. Nothing in the AST is modified.
    fn transform<'a>(
        &self,
        _arena: &'a comrak::Arena<'a>,
        _root: &'a comrak::nodes::AstNode<'a>,
    ) -> bool {
        true
    }

    fn head(&self) -> Option<&'static str> {
        Some(HEAD)
    }
}

/// Injected into `<head>` on every rendered page.
///
/// The script is defensive throughout: a browser with no WebMCP support, a
/// `/mcp` endpoint that is unreachable, or a single malformed tool definition
/// must all leave the page working normally. It registers nothing and throws
/// nothing when there is no agent to talk to.
const HEAD: &str = r##"<link rel="alternate" type="text/plain" href="/llms.txt" title="llms.txt">
<script>
(async function () {
  "use strict";
  var mc = (typeof document !== "undefined" && document.modelContext) ||
           (typeof navigator !== "undefined" && navigator.modelContext);
  if (!mc || typeof mc.registerTool !== "function") return;

  var VERSION = "2026-07-28";

  async function rpc(method, params, name) {
    var headers = {
      "Content-Type": "application/json",
      "MCP-Protocol-Version": VERSION,
      "Mcp-Method": method
    };
    if (name) headers["Mcp-Name"] = name;
    var res = await fetch("/mcp", {
      method: "POST",
      headers: headers,
      credentials: "same-origin",
      body: JSON.stringify({ jsonrpc: "2.0", id: Date.now(), method: method, params: params })
    });
    return res.json();
  }

  var listed;
  try {
    listed = await rpc("tools/list", {});
  } catch (e) {
    return;
  }
  var tools = (listed && listed.result && listed.result.tools) || [];

  for (var i = 0; i < tools.length; i++) {
    (function (tool) {
      try {
        mc.registerTool({
          name: tool.name,
          description: tool.description,
          inputSchema: tool.inputSchema,
          execute: async function (args) {
            try {
              var msg = await rpc("tools/call", { name: tool.name, arguments: args || {} }, tool.name);
              if (msg && msg.error) {
                return { content: [{ type: "text", text: msg.error.message }], isError: true };
              }
              return msg.result;
            } catch (e) {
              return { content: [{ type: "text", text: String(e) }], isError: true };
            }
          }
        });
      } catch (e) {
        /* One rejected tool definition must not cost the others. */
      }
    })(tools[i]);
  }
})();
</script>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Set;

    fn set() -> Set {
        Set::resolve(&["webmcp".to_string()]).unwrap()
    }

    #[test]
    fn the_plugin_is_registered_and_described() {
        assert_eq!(WebMcp.name(), "webmcp");
        assert!(WebMcp.describe().contains("MCP"));
        assert!(crate::plugin::catalog().iter().any(|l| l.starts_with("webmcp — ")));
    }

    #[test]
    fn heading_ids_are_emitted_so_anchors_resolve() {
        let html = set().render_html("# Getting Started\n").html;
        assert!(html.contains(r#"id="getting-started""#), "got: {html}");
    }

    #[test]
    fn ids_are_absent_without_the_plugin() {
        let html = Set::default().render_html("# Getting Started\n").html;
        assert!(!html.contains("id="), "got: {html}");
    }

    #[test]
    fn the_script_is_attached_to_every_page_not_just_ones_it_changed() {
        // Contrast with math and mermaid, which contribute head markup only on
        // pages holding a formula or a diagram.
        let prose = set().render_html("just prose, nothing special\n");
        assert!(!prose.head.is_empty());
        assert!(prose.head.contains("modelContext"));
    }

    #[test]
    fn the_script_falls_back_to_the_shipped_chrome_api() {
        assert!(HEAD.contains("document.modelContext"));
        assert!(HEAD.contains("navigator.modelContext"));
        assert!(HEAD.contains("registerTool"));
    }

    #[test]
    fn the_script_sends_the_headers_the_transport_requires() {
        // 2026-07-28 makes MCP-Protocol-Version and Mcp-Method mandatory, and
        // Mcp-Name mandatory for tools/call. The server enforces this against
        // itself, so a regression here fails the round trip.
        assert!(HEAD.contains("MCP-Protocol-Version"));
        assert!(HEAD.contains("Mcp-Method"));
        assert!(HEAD.contains("Mcp-Name"));
        assert!(HEAD.contains("2026-07-28"));
    }

    #[test]
    fn the_script_announces_the_llms_file() {
        assert!(HEAD.contains(r#"href="/llms.txt""#));
    }

    #[test]
    fn the_head_markup_is_balanced() {
        assert_eq!(HEAD.matches("<script").count(), 1);
        assert_eq!(HEAD.matches("</script>").count(), 1);
    }
}
