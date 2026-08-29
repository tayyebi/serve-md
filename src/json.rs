//! JSON, hand-rolled.
//!
//! MCP speaks JSON-RPC, so the server has to parse and emit JSON. Reaching for
//! `serde_json` would end the one-dependency property this crate advertises, so
//! — as with the template engine, the HTML tokenizer, base64 and
//! percent-encoding — it is written here instead.
//!
//! The parser is total on `&str` input: every failure is an `Error`, never a
//! panic.
//!
//! # Standard
//!
//! RFC 8259, "The JavaScript Object Notation (JSON) Data Interchange Format".
//! <https://www.rfc-editor.org/rfc/rfc8259>
//!
//! Deliberate departures from the grammar, both in the direction of refusing
//! input RFC 8259 permits:
//!
//! - Nesting is capped (see [`MAX_DEPTH`]). §9 explicitly allows a parser to
//!   "set limits on the maximum depth of nesting".
//! - Leading zeros and a bare `-` are accepted by [`Parser::number`] and then
//!   rejected by Rust's `f64` parser rather than by the scanner, so the error
//!   is reported one position later than §6's grammar would place it.

use std::fmt::Write as _;

/// The deepest nesting of arrays and objects that will be parsed.
///
/// `Parser::value` recurses once per level, so without a ceiling a short body
/// made entirely of opening brackets would overflow the stack — a crash an
/// unauthenticated client could trigger at will.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    /// A `Vec` rather than a map: key order is preserved, so output is
    /// deterministic and can be compared byte-for-byte in tests, and nothing
    /// here is large enough for lookup cost to matter.
    Obj(Vec<(String, Value)>),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub msg: String,
    pub at: usize,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.at)
    }
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    pub fn int(n: i64) -> Value {
        Value::Num(n as f64)
    }

    /// Builds an object from a literal array, so response shapes read close to
    /// the JSON they produce: `Value::obj([("code", Value::int(-32601))])`.
    pub fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
        Value::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().filter(|n| n.is_finite()).map(|n| n as i64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Read by tests across several modules. The server itself only ever
    /// writes arrays — nothing it parses from a request contains one — so this
    /// has no caller in the binary.
    #[allow(dead_code)]
    pub fn as_arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

pub fn write(v: &Value) -> String {
    let mut out = String::new();
    write_into(v, &mut out);
    out
}

fn write_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Num(n) => write_num(*n, out),
        Value::Str(s) => write_str(s, out),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_into(item, out);
            }
            out.push(']');
        }
        Value::Obj(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(k, out);
                out.push(':');
                write_into(val, out);
            }
            out.push('}');
        }
    }
}

/// Whole numbers are written without a fractional part, so a count comes out
/// as `3` rather than `3.0` — some MCP clients validate integer fields
/// strictly. The bound is the largest integer an `f64` represents exactly.
fn write_num(n: f64, out: &mut String) {
    if !n.is_finite() {
        // JSON has no NaN or infinity, and null is the only honest encoding.
        out.push_str("null");
    } else if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn parse(src: &str) -> Result<Value> {
    let mut p = Parser {
        b: src.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing input"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> Error {
        Error {
            msg: msg.to_string(),
            at: self.i,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Result<()> {
        if self.peek() == Some(byte) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", byte as char)))
        }
    }

    fn lit(&mut self, word: &str, v: Value) -> Result<Value> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'n') => self.lit("null", Value::Null),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Arr(items));
                }
                _ => return Err(self.err("expected `,` or `]`")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value> {
        self.eat(b'{')?;
        let mut pairs = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Obj(pairs));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            self.eat(b':')?;
            self.ws();
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Obj(pairs));
                }
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.eat(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| self.err("invalid utf-8")),
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.i += 1;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let ch = self.unicode_escape()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                // Unescaped control bytes are not legal inside a JSON string.
                0x00..=0x1f => return Err(self.err("control character in string")),
                // Everything else is copied through. The input was a `&str`
                // and every byte consumed above is ASCII, so whole UTF-8
                // sequences are copied intact and the result stays valid.
                _ => out.push(c),
            }
        }
    }

    /// One `\uXXXX` escape, joining a surrogate pair with the `\uXXXX` that
    /// must follow when the first half is a high surrogate — otherwise
    /// characters outside the BMP would decode to an unpaired surrogate,
    /// which is not a `char`.
    fn unicode_escape(&mut self) -> Result<char> {
        let first = self.hex4()?;
        if (0xd800..0xdc00).contains(&first) {
            if self.peek() != Some(b'\\') || self.b.get(self.i + 1) != Some(&b'u') {
                return Err(self.err("unpaired high surrogate"));
            }
            self.i += 2;
            let second = self.hex4()?;
            if !(0xdc00..0xe000).contains(&second) {
                return Err(self.err("invalid low surrogate"));
            }
            let c = 0x1_0000 + ((first - 0xd800) << 10) + (second - 0xdc00);
            return char::from_u32(c).ok_or_else(|| self.err("invalid code point"));
        }
        char::from_u32(first).ok_or_else(|| self.err("invalid code point"))
    }

    fn hex4(&mut self) -> Result<u32> {
        let end = self.i + 4;
        if end > self.b.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let mut n = 0u32;
        for &byte in &self.b[self.i..end] {
            let d = (byte as char)
                .to_digit(16)
                .ok_or_else(|| self.err("invalid hex digit"))?;
            n = n * 16 + d;
        }
        self.i = end;
        Ok(n)
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        self.digits();
        if self.peek() == Some(b'.') {
            self.i += 1;
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            self.digits();
        }
        let text = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| self.err("invalid number"))?;
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| self.err("invalid number"))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse(" true ").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("-12.5e2").unwrap(), Value::Num(-1250.0));
        assert_eq!(parse("\"hi\"").unwrap(), Value::str("hi"));
    }

    #[test]
    fn nested_structures() {
        let v = parse(r#"{"a":[1,2,{"b":null}],"c":true}"#).unwrap();
        assert_eq!(v.get("a").unwrap().as_arr().unwrap().len(), 3);
        assert_eq!(v.get("c").unwrap().as_bool(), Some(true));
        assert!(v.get("a").unwrap().as_arr().unwrap()[2]
            .get("b")
            .unwrap()
            .is_null());
        assert!(v.get("missing").is_none());
    }

    #[test]
    fn empty_containers() {
        assert_eq!(parse("[]").unwrap(), Value::Arr(vec![]));
        assert_eq!(parse("{}").unwrap(), Value::Obj(vec![]));
        assert_eq!(parse(" [ ] ").unwrap(), Value::Arr(vec![]));
    }

    #[test]
    fn escapes_round_trip() {
        let src = "\"a\\\"b\\\\c\\nd\\te\\u0041\"";
        assert_eq!(parse(src).unwrap().as_str(), Some("a\"b\\c\nd\teA"));
        let v = Value::str("quote\" back\\ nl\n tab\t bell\u{7}");
        assert_eq!(parse(&write(&v)).unwrap(), v);
    }

    #[test]
    fn surrogate_pairs_join() {
        // U+1F600, as the pair a JSON encoder would emit.
        assert_eq!(parse(r#""\ud83d\ude00""#).unwrap().as_str(), Some("😀"));
        assert!(parse(r#""\ud83d""#).is_err(), "unpaired high surrogate");
        assert!(parse(r#""\ude00""#).is_err(), "lone low surrogate");
        assert!(parse(r#""\ud83dA""#).is_err(), "high surrogate not followed by \\u");
    }

    #[test]
    fn writes_whole_numbers_without_a_fraction() {
        assert_eq!(write(&Value::int(3)), "3");
        assert_eq!(write(&Value::Num(3.5)), "3.5");
        assert_eq!(write(&Value::Num(f64::NAN)), "null");
        assert_eq!(write(&Value::Num(f64::INFINITY)), "null");
    }

    #[test]
    fn object_key_order_is_preserved() {
        let v = Value::obj([("z", Value::int(1)), ("a", Value::int(2))]);
        assert_eq!(write(&v), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn round_trips_a_whole_document() {
        let src = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[],"ok":true}}"#;
        assert_eq!(write(&parse(src).unwrap()), src);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        for bad in [
            "", "{", "[", "}", "\"unterminated", "{\"a\"}", "{\"a\":}", "[1,]", "[1 2]",
            "tru", "nul", "{\"a\":1}x", "\"\u{1}\"", "\"\\q\"", "\"\\u00\"", "@",
        ] {
            assert!(parse(bad).is_err(), "expected an error for {bad:?}");
        }
    }

    #[test]
    fn nesting_is_capped() {
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        let err = parse(&deep).unwrap_err();
        assert!(err.msg.contains("nesting too deep"));
        // One level under the cap still parses.
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn errors_report_a_position() {
        let err = parse(r#"{"a":@}"#).unwrap_err();
        assert_eq!(err.at, 5);
        assert!(err.to_string().contains("byte 5"));
    }
}
