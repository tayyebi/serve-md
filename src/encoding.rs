pub fn percent_encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encodes for an RFC 8187 `ext-value`, whose `attr-char` set is
/// narrower than a path's: everything outside it is escaped, so the result is
/// safe as a header parameter value.
///
/// Separate from [`percent_encode_path`] because that one deliberately leaves
/// `/` alone, and a header value has no path structure to preserve.
pub fn percent_encode_attr(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'!' | b'#' | b'$' | b'&' | b'+' | b'-'
            | b'.' | b'^' | b'_' | b'`' | b'|' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(());
                }
                out.push(hex_value(bytes[i + 1])? * 16 + hex_value(bytes[i + 2])?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex_value(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    let mut val: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::new();
    for &b in input.as_bytes() {
        let d = match b {
            b'A'..=b'Z' => u32::from(b - b'A'),
            b'a'..=b'z' => u32::from(b - b'a') + 26,
            b'0'..=b'9' => u32::from(b - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err(()),
        };
        val = (val << 6) | d;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((val >> bits) as u8);
            val &= (1u32 << bits) - 1;
        }
    }
    Ok(out)
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_roundtrip() {
        for s in [
            "docs/guide file#2.md",
            "a b c",
            "100%",
            "ünïcode.md",
            "plain.md",
        ] {
            let enc = percent_encode_path(s);
            assert!(!enc.contains(' '));
            assert_eq!(percent_decode(&enc).unwrap(), s);
        }
    }

    #[test]
    fn attr_encoding_escapes_everything_outside_attr_char() {
        assert_eq!(percent_encode_attr("Caf\u{e9} Guide"), "Caf%C3%A9%20Guide");
        // `/` survives percent_encode_path but not this one.
        assert_eq!(percent_encode_attr("a/b"), "a%2Fb");
        assert_eq!(percent_encode_attr("ok-name~1"), "ok-name~1");
    }

    #[test]
    fn percent_rejects_malformed() {
        assert!(percent_decode("%zz").is_err());
        assert!(percent_decode("%2").is_err());
        assert!(percent_decode("%00%").is_err());
    }

    #[test]
    fn base64_basic() {
        assert_eq!(base64_decode("dTpw").unwrap(), b"u:p");
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
    }

    #[test]
    fn base64_padding_stops() {
        assert_eq!(base64_decode("dXNlcjpwYXNz====").unwrap(), b"user:pass");
    }

    #[test]
    fn constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(&[], b"a"));
    }
}
