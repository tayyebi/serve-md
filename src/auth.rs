use crate::encoding::{base64_decode, constant_time_eq};

pub struct Auth {
    user: String,
    pass: String,
}

impl Auth {
    pub fn new(user: String, pass: String) -> Self {
        Self { user, pass }
    }

    pub fn check(&self, headers: &[(String, String)]) -> bool {
        let Some(hdr) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        else {
            return false;
        };
        let value = hdr.1.trim();
        let Some(b64) = value
            .strip_prefix("Basic ")
            .or_else(|| value.strip_prefix("basic "))
        else {
            return false;
        };
        let Ok(decoded) = base64_decode(b64.trim()) else {
            return false;
        };
        let mut expected = String::with_capacity(self.user.len() + self.pass.len() + 1);
        expected.push_str(&self.user);
        expected.push(':');
        expected.push_str(&self.pass);
        constant_time_eq(&decoded, expected.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(value: &str) -> Vec<(String, String)> {
        vec![("Authorization".to_string(), value.to_string())]
    }

    #[test]
    fn accepts_correct_credentials() {
        let a = Auth::new("u".into(), "p".into());
        assert!(a.check(&hdr("Basic dTpw")));
        assert!(a.check(&hdr("basic dTpw")));
        assert!(a.check(&hdr("Basic dTpw ")));
    }

    #[test]
    fn rejects_wrong_and_missing() {
        let a = Auth::new("u".into(), "p".into());
        assert!(!a.check(&hdr("Basic dWtub3du")));
        assert!(!a.check(&hdr("Bearer dTpw")));
        assert!(!a.check(&hdr("Basic not-base64!")));
        assert!(!a.check(&[]));
    }
}
