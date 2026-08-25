//! Server-side LaTeX math, rendered to MathML.
//!
//! comrak already *parses* math when `math_dollars`/`math_code` are on, but it
//! only emits the escaped LaTeX inside a `<span data-math-style=…>`. This
//! plugin does the typesetting: it walks the AST and replaces each math node
//! with MathML, which browsers render natively — no JavaScript, no web fonts,
//! no CDN, and no dependency beyond the Markdown parser.
//!
//! The converter is a recursive-descent parser over a practical subset of
//! LaTeX math: scripts, fractions, roots, big operators with limits, stretchy
//! delimiters, text runs, font variants and around 150 symbols. Anything it
//! does not understand makes it give up on that formula, which leaves comrak's
//! escaped source in place rather than emitting something wrong.

use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, Options};

use super::Plugin;
use crate::template::escape_html;

/// The only CSS the math plugin needs, shipped only on pages containing math.
/// Browsers style MathML themselves; this keeps a long display equation
/// scrollable instead of letting it overflow a narrow viewport.
const STYLE: &str = concat!(
    "<style>",
    "math{font-size:1.1em}",
    "math[display=\"block\"]{display:block;overflow-x:auto;overflow-y:hidden;margin:1em 0}",
    "</style>"
);

pub struct Math;

impl Plugin for Math {
    fn name(&self) -> &'static str {
        "math"
    }

    fn describe(&self) -> &'static str {
        "render LaTeX math as MathML"
    }

    fn configure(&self, options: &mut Options<'_>) {
        options.extension.math_dollars = true; // $x$ and $$x$$
        options.extension.math_code = true; // $`x`$ and ```math fences
    }

    fn transform<'a>(&self, _arena: &'a Arena<'a>, root: &'a AstNode<'a>) -> bool {
        let mut found = false;
        for node in root.descendants() {
            // Scoped so the shared borrow is released before the mutation below.
            let replacement = {
                let ast = node.data.borrow();
                match &ast.value {
                    NodeValue::Math(m) => to_mathml(&m.literal, m.display_math),
                    // A ```math fence arrives as a CodeBlock, not a Math node.
                    // comrak treats the first whitespace-delimited token of
                    // `info` as the language.
                    NodeValue::CodeBlock(cb)
                        if cb.info.split_whitespace().next() == Some("math") =>
                    {
                        to_mathml(&cb.literal, true)
                    }
                    _ => None,
                }
            };
            if let Some(mathml) = replacement {
                // `Raw` rather than `HtmlInline`: with `render.unsafe_` off —
                // which is what keeps author-written HTML escaped — comrak
                // replaces `HtmlInline` with `<!-- raw HTML omitted -->`, while
                // `Raw` is written verbatim. It exists for exactly this case:
                // markup produced by the renderer rather than by the document.
                node.data.borrow_mut().value = NodeValue::Raw(mathml);
                found = true;
            }
        }
        found
    }

    fn head(&self) -> Option<&'static str> {
        Some(STYLE)
    }
}

/// Guards against a pathological input like `$among {{{{…` exhausting the
/// stack. This is a web server: the formula comes from whatever file is served.
const MAX_DEPTH: u32 = 48;
/// Formulas longer than this are almost certainly not formulas.
const MAX_LEN: usize = 8192;

/// Converts one LaTeX fragment to MathML, or `None` if it does not parse.
///
/// Returning `None` leaves the node untouched, so comrak falls back to its
/// escaped `<span data-math-style=…>`: a malformed formula shows up as visible
/// LaTeX source instead of breaking the page.
pub(super) fn to_mathml(latex: &str, display: bool) -> Option<String> {
    let latex = latex.trim();
    if latex.is_empty() || latex.len() > MAX_LEN {
        return None;
    }
    let mut p = Parser {
        c: latex.chars().collect(),
        i: 0,
        depth: 0,
        display,
    };
    let body = p.row(Stop::Eof)?;
    if p.i < p.c.len() {
        return None; // trailing `}` or `\right` with no opener
    }
    let mode = if display { "block" } else { "inline" };
    Some(format!(
        "<math display=\"{mode}\"><semantics><mrow>{body}</mrow>\
         <annotation encoding=\"application/x-tex\">{}</annotation></semantics></math>",
        // Every piece of user text in this file goes through `escape_html`;
        // the result is inserted via a `Raw` node that bypasses comrak's own
        // escaping, so nothing may reach the page unescaped.
        escape_html(latex)
    ))
}

// ---------------------------------------------------------------- parsing

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Eof,
    Brace,
    Right,
}

/// One parsed element, plus whether it is a big operator — which decides
/// whether its scripts sit above and below or beside it.
struct Node {
    html: String,
    big: bool,
}

impl Node {
    fn plain(html: String) -> Self {
        Self { html, big: false }
    }
}

struct Parser {
    c: Vec<char>,
    i: usize,
    depth: u32,
    display: bool,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.i += 1;
        }
        ch
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.i += 1;
        }
    }

    /// Reads a control sequence, the backslash already consumed. A non-letter
    /// after the backslash is itself the command, as in `\,` or `\{`.
    fn command(&mut self) -> Option<String> {
        let first = self.bump()?;
        if !first.is_ascii_alphabetic() {
            return Some(first.to_string());
        }
        let mut name = String::from(first);
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            name.push(self.bump()?);
        }
        Some(name)
    }

    /// A sequence of elements, up to whatever ends this row.
    fn row(&mut self, stop: Stop) -> Option<String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let mut out = String::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    // Only the top level may end here; a group or `\left`
                    // reaching end-of-input is unbalanced.
                    if stop != Stop::Eof {
                        return None;
                    }
                    break;
                }
                Some('}') => {
                    if stop != Stop::Brace {
                        return None; // `}` with no `{`
                    }
                    break; // caller consumes it
                }
                Some('\\') => {
                    let save = self.i;
                    self.i += 1;
                    let cmd = self.command()?;
                    if cmd == "right" {
                        if stop != Stop::Right {
                            return None;
                        }
                        break; // caller reads the closing delimiter
                    }
                    self.i = save;
                    out.push_str(&self.scripted()?);
                }
                _ => out.push_str(&self.scripted()?),
            }
        }
        self.depth -= 1;
        Some(out)
    }

    /// One element together with any `_` and `^` attached to it.
    fn scripted(&mut self) -> Option<String> {
        let base = self.atom()?;
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('_') if sub.is_none() => {
                    self.i += 1;
                    sub = Some(self.group()?);
                }
                Some('^') if sup.is_none() => {
                    self.i += 1;
                    sup = Some(self.group()?);
                }
                _ => break,
            }
        }
        // Limits go above and below a big operator, but only in display mode —
        // inline, that would blow up the line height.
        let stacked = base.big && self.display;
        let b = base.html;
        Some(match (sub, sup, stacked) {
            (None, None, _) => b,
            (Some(d), None, true) => format!("<munder>{b}{d}</munder>"),
            (None, Some(u), true) => format!("<mover>{b}{u}</mover>"),
            (Some(d), Some(u), true) => format!("<munderover>{b}{d}{u}</munderover>"),
            (Some(d), None, false) => format!("<msub>{b}{d}</msub>"),
            (None, Some(u), false) => format!("<msup>{b}{u}</msup>"),
            (Some(d), Some(u), false) => format!("<msubsup>{b}{d}{u}</msubsup>"),
        })
    }

    /// A braced group, or a single element if there are no braces — so both
    /// `x^{10}` and `x^2` work.
    fn group(&mut self) -> Option<String> {
        self.skip_ws();
        if self.peek() == Some('{') {
            self.i += 1;
            let inner = self.row(Stop::Brace)?;
            if self.bump() != Some('}') {
                return None;
            }
            return Some(format!("<mrow>{inner}</mrow>"));
        }
        Some(self.atom()?.html)
    }

    /// The raw text of a braced group, for `\text{…}` where the contents are
    /// not math. Nested braces are kept balanced but not interpreted.
    fn raw_group(&mut self) -> Option<String> {
        self.skip_ws();
        if self.bump() != Some('{') {
            return None;
        }
        let mut out = String::new();
        let mut depth = 1;
        loop {
            match self.bump()? {
                '{' => {
                    depth += 1;
                    out.push('{');
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                    out.push('}');
                }
                c => out.push(c),
            }
        }
    }

    /// A single delimiter after `\left` or `\right`.
    fn delimiter(&mut self) -> Option<String> {
        self.skip_ws();
        let ch = self.bump()?;
        if ch == '\\' {
            let cmd = self.command()?;
            return match cmd.as_str() {
                "{" => Some("{".into()),
                "}" => Some("}".into()),
                "|" => Some("‖".into()),
                "langle" => Some("⟨".into()),
                "rangle" => Some("⟩".into()),
                "lceil" => Some("⌈".into()),
                "rceil" => Some("⌉".into()),
                "lfloor" => Some("⌊".into()),
                "rfloor" => Some("⌋".into()),
                _ => None,
            };
        }
        match ch {
            '(' | ')' | '[' | ']' | '|' | '/' => Some(ch.to_string()),
            // `\left.` is an intentionally invisible delimiter.
            '.' => Some(String::new()),
            _ => None,
        }
    }
}

impl Parser {
    /// One element, without its scripts.
    fn atom(&mut self) -> Option<Node> {
        self.skip_ws();
        let ch = self.peek()?;
        if ch.is_ascii_digit() {
            return Some(Node::plain(format!("<mn>{}</mn>", self.number())));
        }
        if ch.is_alphabetic() {
            self.i += 1;
            return Some(Node::plain(format!("<mi>{}</mi>", escape_html(&ch.to_string()))));
        }
        if ch == '{' {
            self.i += 1;
            let inner = self.row(Stop::Brace)?;
            if self.bump() != Some('}') {
                return None;
            }
            return Some(Node::plain(format!("<mrow>{inner}</mrow>")));
        }
        if ch == '\\' {
            self.i += 1;
            let cmd = self.command()?;
            return self.control(&cmd);
        }
        self.i += 1;
        match ch {
            '\'' => Some(Node::plain("<mo>\u{2032}</mo>".into())),
            // Anything left is an operator or punctuation. `escape_html`
            // matters here: `<`, `>` and `&` are all valid LaTeX operators.
            _ => Some(Node::plain(format!(
                "<mo>{}</mo>",
                escape_html(&operator(ch).to_string())
            ))),
        }
    }

    fn number(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            // A dot belongs to the number only when a digit follows it, so
            // `1.` and `f.x` are left alone.
            let decimal_point =
                c == '.' && matches!(self.c.get(self.i + 1), Some(d) if d.is_ascii_digit());
            if !c.is_ascii_digit() && !decimal_point {
                break;
            }
            out.push(c);
            self.i += 1;
        }
        out
    }

    /// Dispatches a control sequence.
    fn control(&mut self, cmd: &str) -> Option<Node> {
        match cmd {
            "frac" | "dfrac" | "tfrac" => {
                let num = self.group()?;
                let den = self.group()?;
                Some(Node::plain(format!("<mfrac>{num}{den}</mfrac>")))
            }
            "sqrt" => {
                self.skip_ws();
                if self.peek() == Some('[') {
                    self.i += 1;
                    let mut index = String::new();
                    loop {
                        match self.bump()? {
                            ']' => break,
                            c => index.push(c),
                        }
                    }
                    let root = to_index(&index)?;
                    let radicand = self.group()?;
                    return Some(Node::plain(format!("<mroot>{radicand}{root}</mroot>")));
                }
                let radicand = self.group()?;
                Some(Node::plain(format!("<msqrt>{radicand}</msqrt>")))
            }
            "text" | "textrm" | "textnormal" | "mbox" => {
                let body = self.raw_group()?;
                Some(Node::plain(format!("<mtext>{}</mtext>", escape_html(&body))))
            }
            "mathrm" | "mathbf" | "mathit" | "mathbb" | "mathcal" | "mathsf" | "mathtt"
            | "boldsymbol" | "mathfrak" => {
                let body = self.raw_group()?;
                let variant = match cmd {
                    "mathbf" | "boldsymbol" => "bold",
                    "mathit" => "italic",
                    "mathbb" => "double-struck",
                    "mathcal" => "script",
                    "mathsf" => "sans-serif",
                    "mathtt" => "monospace",
                    "mathfrak" => "fraktur",
                    _ => "normal",
                };
                Some(Node::plain(format!(
                    "<mi mathvariant=\"{variant}\">{}</mi>",
                    escape_html(&body)
                )))
            }
            "left" => {
                let open = self.delimiter()?;
                let inner = self.row(Stop::Right)?;
                let close = self.delimiter()?;
                Some(Node::plain(format!(
                    "<mrow>{}{inner}{}</mrow>",
                    stretchy(&open),
                    stretchy(&close)
                )))
            }
            // Explicit spacing.
            "," | ":" | ";" | ">" => Some(Node::plain("<mspace width=\"0.22em\"/>".into())),
            "!" => Some(Node::plain("<mspace width=\"-0.17em\"/>".into())),
            "quad" => Some(Node::plain("<mspace width=\"1em\"/>".into())),
            "qquad" => Some(Node::plain("<mspace width=\"2em\"/>".into())),
            " " => Some(Node::plain("<mspace width=\"0.25em\"/>".into())),
            // Escaped literals.
            "{" | "}" | "%" | "$" | "#" | "&" | "_" => Some(Node::plain(format!(
                "<mo>{}</mo>",
                escape_html(cmd)
            ))),
            _ => {
                if let Some(name) = function(cmd) {
                    let big = matches!(cmd, "lim" | "max" | "min" | "sup" | "inf" | "limsup" | "liminf");
                    return Some(Node {
                        html: format!("<mi>{name}</mi>"),
                        big,
                    });
                }
                let (tag, glyph, big) = symbol(cmd)?;
                let attr = if big { " largeop=\"true\"" } else { "" };
                Some(Node {
                    html: format!("<{tag}{attr}>{glyph}</{tag}>"),
                    big,
                })
            }
        }
    }
}

/// A root index like `[3]`, which is a bare number or identifier.
fn to_index(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return Some(format!("<mn>{s}</mn>"));
    }
    if s.chars().all(char::is_alphanumeric) {
        return Some(format!("<mi>{}</mi>", escape_html(s)));
    }
    None
}

fn stretchy(delim: &str) -> String {
    if delim.is_empty() {
        return String::new();
    }
    format!("<mo stretchy=\"true\">{}</mo>", escape_html(delim))
}

/// Maps ASCII operators onto their proper typographic glyphs.
fn operator(c: char) -> char {
    match c {
        '-' => '\u{2212}', // minus sign, not hyphen
        '*' => '\u{2217}', // asterisk operator
        _ => c,
    }
}

/// Function names set upright rather than italic.
fn function(cmd: &str) -> Option<&'static str> {
    const NAMES: &[&str] = &[
        "sin", "cos", "tan", "sec", "csc", "cot", "arcsin", "arccos", "arctan", "sinh", "cosh",
        "tanh", "coth", "log", "ln", "lg", "exp", "det", "dim", "ker", "deg", "gcd", "hom", "arg",
        "Pr", "lim", "max", "min", "sup", "inf", "limsup", "liminf",
    ];
    NAMES.iter().find(|n| **n == cmd).copied()
}

/// Symbol lookup: `(MathML tag, glyph, is a big operator)`.
///
/// Glyphs are written as escapes so this table stays readable in any editor
/// and cannot pick up a stray byte. Everything here is a compile-time
/// constant, so none of it needs escaping on the way out.
fn symbol(cmd: &str) -> Option<(&'static str, &'static str, bool)> {
    let mi = |g| Some(("mi", g, false));
    let mo = |g| Some(("mo", g, false));
    let big = |g| Some(("mo", g, true));
    match cmd {
        // Lowercase Greek.
        "alpha" => mi("\u{3b1}"),
        "beta" => mi("\u{3b2}"),
        "gamma" => mi("\u{3b3}"),
        "delta" => mi("\u{3b4}"),
        "epsilon" => mi("\u{3f5}"),
        "varepsilon" => mi("\u{3b5}"),
        "zeta" => mi("\u{3b6}"),
        "eta" => mi("\u{3b7}"),
        "theta" => mi("\u{3b8}"),
        "vartheta" => mi("\u{3d1}"),
        "iota" => mi("\u{3b9}"),
        "kappa" => mi("\u{3ba}"),
        "lambda" => mi("\u{3bb}"),
        "mu" => mi("\u{3bc}"),
        "nu" => mi("\u{3bd}"),
        "xi" => mi("\u{3be}"),
        "pi" => mi("\u{3c0}"),
        "varpi" => mi("\u{3d6}"),
        "rho" => mi("\u{3c1}"),
        "varrho" => mi("\u{3f1}"),
        "sigma" => mi("\u{3c3}"),
        "varsigma" => mi("\u{3c2}"),
        "tau" => mi("\u{3c4}"),
        "upsilon" => mi("\u{3c5}"),
        "phi" => mi("\u{3d5}"),
        "varphi" => mi("\u{3c6}"),
        "chi" => mi("\u{3c7}"),
        "psi" => mi("\u{3c8}"),
        "omega" => mi("\u{3c9}"),
        // Uppercase Greek.
        "Gamma" => mi("\u{393}"),
        "Delta" => mi("\u{394}"),
        "Theta" => mi("\u{398}"),
        "Lambda" => mi("\u{39b}"),
        "Xi" => mi("\u{39e}"),
        "Pi" => mi("\u{3a0}"),
        "Sigma" => mi("\u{3a3}"),
        "Upsilon" => mi("\u{3a5}"),
        "Phi" => mi("\u{3a6}"),
        "Psi" => mi("\u{3a8}"),
        "Omega" => mi("\u{3a9}"),
        // Big operators.
        "sum" => big("\u{2211}"),
        "prod" => big("\u{220f}"),
        "coprod" => big("\u{2210}"),
        "int" => big("\u{222b}"),
        "iint" => big("\u{222c}"),
        "iiint" => big("\u{222d}"),
        "oint" => big("\u{222e}"),
        "bigcup" => big("\u{22c3}"),
        "bigcap" => big("\u{22c2}"),
        "bigoplus" => big("\u{2a01}"),
        "bigotimes" => big("\u{2a02}"),
        "bigvee" => big("\u{22c1}"),
        "bigwedge" => big("\u{22c0}"),
        // Binary operators.
        "times" => mo("\u{d7}"),
        "div" => mo("\u{f7}"),
        "pm" => mo("\u{b1}"),
        "mp" => mo("\u{2213}"),
        "cdot" => mo("\u{22c5}"),
        "ast" => mo("\u{2217}"),
        "star" => mo("\u{22c6}"),
        "circ" => mo("\u{2218}"),
        "bullet" => mo("\u{2219}"),
        "oplus" => mo("\u{2295}"),
        "ominus" => mo("\u{2296}"),
        "otimes" => mo("\u{2297}"),
        "oslash" => mo("\u{2298}"),
        "cup" => mo("\u{222a}"),
        "cap" => mo("\u{2229}"),
        "setminus" => mo("\u{2216}"),
        "wedge" | "land" => mo("\u{2227}"),
        "vee" | "lor" => mo("\u{2228}"),
        // Relations.
        "leq" | "le" => mo("\u{2264}"),
        "geq" | "ge" => mo("\u{2265}"),
        "neq" | "ne" => mo("\u{2260}"),
        "equiv" => mo("\u{2261}"),
        "approx" => mo("\u{2248}"),
        "cong" => mo("\u{2245}"),
        "sim" => mo("\u{223c}"),
        "simeq" => mo("\u{2243}"),
        "propto" => mo("\u{221d}"),
        "ll" => mo("\u{226a}"),
        "gg" => mo("\u{226b}"),
        "subset" => mo("\u{2282}"),
        "supset" => mo("\u{2283}"),
        "subseteq" => mo("\u{2286}"),
        "supseteq" => mo("\u{2287}"),
        "in" => mo("\u{2208}"),
        "notin" => mo("\u{2209}"),
        "ni" => mo("\u{220b}"),
        "perp" => mo("\u{22a5}"),
        "parallel" => mo("\u{2225}"),
        "mid" => mo("\u{2223}"),
        // Arrows.
        "rightarrow" | "to" => mo("\u{2192}"),
        "leftarrow" | "gets" => mo("\u{2190}"),
        "leftrightarrow" => mo("\u{2194}"),
        "Rightarrow" | "implies" => mo("\u{21d2}"),
        "Leftarrow" => mo("\u{21d0}"),
        "Leftrightarrow" | "iff" => mo("\u{21d4}"),
        "mapsto" => mo("\u{21a6}"),
        "uparrow" => mo("\u{2191}"),
        "downarrow" => mo("\u{2193}"),
        "longrightarrow" => mo("\u{27f6}"),
        "longleftarrow" => mo("\u{27f5}"),
        // Miscellaneous.
        "infty" => mi("\u{221e}"),
        "partial" => mi("\u{2202}"),
        "nabla" => mi("\u{2207}"),
        "forall" => mo("\u{2200}"),
        "exists" => mo("\u{2203}"),
        "nexists" => mo("\u{2204}"),
        "neg" | "lnot" => mo("\u{ac}"),
        "emptyset" | "varnothing" => mi("\u{2205}"),
        "aleph" => mi("\u{2135}"),
        "hbar" => mi("\u{210f}"),
        "ell" => mi("\u{2113}"),
        "Re" => mi("\u{211c}"),
        "Im" => mi("\u{2111}"),
        "wp" => mi("\u{2118}"),
        "prime" => mo("\u{2032}"),
        "degree" => mo("\u{b0}"),
        "angle" => mo("\u{2220}"),
        "triangle" => mo("\u{25b3}"),
        "square" => mo("\u{25a1}"),
        "therefore" => mo("\u{2234}"),
        "because" => mo("\u{2235}"),
        "dots" | "ldots" => mo("\u{2026}"),
        "cdots" => mo("\u{22ef}"),
        "vdots" => mo("\u{22ee}"),
        "ddots" => mo("\u{22f1}"),
        "surd" => mo("\u{221a}"),
        "checkmark" => mo("\u{2713}"),
        "dagger" => mo("\u{2020}"),
        "S" => mo("\u{a7}"),
        "P" => mo("\u{b6}"),
        // Number sets, the common `\mathbb` shorthands.
        "N" => mi("\u{2115}"),
        "Z" => mi("\u{2124}"),
        "Q" => mi("\u{211a}"),
        "R" => mi("\u{211d}"),
        "C" => mi("\u{2102}"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::to_mathml;
    use crate::plugin::Set;

    fn render(src: &str) -> String {
        let set = Set::resolve(&["math".to_string()]).unwrap();
        set.render_html(src).html
    }

    /// The MathML body for one inline formula, annotation stripped.
    fn ml(latex: &str) -> String {
        let out = to_mathml(latex, false).unwrap_or_else(|| panic!("failed to parse: {latex}"));
        out.split("<annotation").next().unwrap().to_string()
    }

    #[test]
    fn inline_dollar_math() {
        let out = render("mass energy $E = mc^2$ here\n");
        assert!(out.contains("<math"), "{out}");
        assert!(out.contains("display=\"inline\""), "{out}");
        assert!(!out.contains("data-math-style"), "{out}");
    }

    #[test]
    fn display_dollar_math() {
        let out = render("$$\\int_0^\\infty e^{-x^2} dx$$\n");
        assert!(out.contains("<math"), "{out}");
        assert!(out.contains("display=\"block\""), "{out}");
    }

    #[test]
    fn math_code_fence() {
        let out = render("```math\nx^2 + y^2 = z^2\n```\n");
        assert!(out.contains("<math"), "{out}");
        assert!(out.contains("display=\"block\""), "{out}");
        assert!(!out.contains("language-math"), "{out}");
    }

    #[test]
    fn inline_code_math() {
        assert!(render("inline $`a + b`$ code math\n").contains("<math"));
    }

    #[test]
    fn source_is_kept_as_an_annotation() {
        assert!(render("$E = mc^2$\n").contains("application/x-tex"));
    }

    #[test]
    fn scripts_become_sub_and_superscripts() {
        assert!(ml("x^2").contains("<msup><mi>x</mi><mn>2</mn></msup>"));
        assert!(ml("x_i").contains("<msub><mi>x</mi><mi>i</mi></msub>"));
        assert!(ml("x_i^2").contains("<msubsup>"));
        // Braces group multi-character scripts.
        assert!(ml("x^{10}").contains("<mn>10</mn>"));
    }

    #[test]
    fn big_operator_limits_depend_on_display_mode() {
        // Display mode stacks limits above and below.
        let block = to_mathml("\\sum_{i=1}^{n} i", true).unwrap();
        assert!(block.contains("<munderover>"), "{block}");
        // Inline they sit beside, so the line height stays sane.
        let inline = to_mathml("\\sum_{i=1}^{n} i", false).unwrap();
        assert!(inline.contains("<msubsup>"), "{inline}");
        assert!(!inline.contains("<munderover>"), "{inline}");
    }

    #[test]
    fn fractions_and_roots() {
        assert!(ml("\\frac{a}{b}").contains("<mfrac>"));
        assert!(ml("\\sqrt{2}").contains("<msqrt>"));
        assert!(ml("\\sqrt[3]{x}").contains("<mroot>"));
    }

    #[test]
    fn greek_and_symbols_resolve() {
        assert!(ml("\\alpha").contains('\u{3b1}'));
        assert!(ml("\\pi").contains('\u{3c0}'));
        assert!(ml("\\infty").contains('\u{221e}'));
        assert!(ml("a \\leq b").contains('\u{2264}'));
        assert!(ml("a \\times b").contains('\u{d7}'));
        // A hyphen must render as a real minus sign.
        assert!(ml("a - b").contains('\u{2212}'));
    }

    #[test]
    fn functions_are_upright() {
        assert!(ml("\\sin x").contains("<mi>sin</mi>"));
        assert!(ml("\\log n").contains("<mi>log</mi>"));
    }

    #[test]
    fn stretchy_delimiters() {
        let out = ml("\\left( \\frac{a}{b} \\right)");
        assert!(out.contains("stretchy=\"true\""), "{out}");
        assert!(out.contains("<mfrac>"), "{out}");
    }

    #[test]
    fn text_runs_are_not_parsed_as_math() {
        let out = ml("\\text{if x > 0}");
        assert!(out.contains("<mtext>"), "{out}");
        assert!(out.contains("&gt;"), "{out}");
    }

    #[test]
    fn font_variants() {
        assert!(ml("\\mathbb{R}").contains("double-struck"));
        assert!(ml("\\mathbf{v}").contains("bold"));
    }

    #[test]
    fn malformed_latex_does_not_break_the_page() {
        let out = render("broken $\\frac{$ formula\n");
        // Whichever fallback fires, the surrounding prose must survive intact
        // and nothing may leak into the page as markup.
        assert!(out.contains("broken"), "{out}");
        assert!(out.contains("formula"), "{out}");
        assert!(!out.contains("<script"), "{out}");
    }

    #[test]
    fn unbalanced_and_unknown_input_is_refused() {
        for bad in ["\\frac{a}", "{a", "a}", "\\notacommand", "\\left(a", "a \\\\ b", "\\"] {
            assert!(to_mathml(bad, false).is_none(), "should not parse: {bad}");
        }
    }

    #[test]
    fn pathological_nesting_is_refused_not_crashed() {
        // A deeply nested formula must not blow the stack: this is a web
        // server, and the formula comes from whatever file is served.
        let deep = format!("{}x{}", "{".repeat(500), "}".repeat(500));
        assert!(to_mathml(&deep, false).is_none());
        assert!(to_mathml(&"x".repeat(super::MAX_LEN + 1), false).is_none());
    }

    #[test]
    fn a_lone_dollar_sign_is_not_math() {
        let out = render("that costs $5 today\n");
        assert!(!out.contains("<math"), "{out}");
        assert!(out.contains("$5"), "{out}");
    }

    #[test]
    fn markup_in_latex_cannot_escape_into_html() {
        // `Raw` bypasses comrak's escaping, so these are the load-bearing
        // checks that nothing user-controlled reaches the page as live markup.
        for src in [
            "$\\text{<script>alert(1)</script>}$\n",
            // Aimed straight at the annotation, which is the one place raw
            // source is echoed back into the document.
            "$\\alpha</annotation><script>alert(1)</script>$\n",
            "```math\n</annotation><script>alert(1)</script>\n```\n",
        ] {
            let out = render(src);
            assert!(!out.contains("<script"), "{out}");
            assert!(!out.contains("</script"), "{out}");
        }
    }

    #[test]
    fn annotation_body_is_escaped() {
        let out = to_mathml("x < y", false).expect("< is valid math");
        let tail = &out[out.find("application/x-tex").expect("annotation present")..];
        let body = &tail[..tail.find("</annotation>").expect("annotation closed")];
        // A bare `<` here would be markup the browser acts on, not LaTeX.
        assert!(!body.contains('<'), "{out}");
        assert!(body.contains("&lt;"), "{out}");
    }

    #[test]
    fn operators_in_the_body_are_escaped_too() {
        let body = ml("a < b > c & d");
        assert!(body.contains("&lt;") && body.contains("&gt;") && body.contains("&amp;"));
        assert!(!body.contains("<mo><"), "{body}");
    }

    #[test]
    fn disabled_without_the_plugin() {
        let out = Set::default().render_html("$E = mc^2$\n").html;
        assert!(!out.contains("<math"), "{out}");
        assert!(out.contains("$E = mc^2$"), "{out}");
    }
}
