//! Small shared helpers.

use std::time::Duration;

/// Non-zero positive draft id for `sendMessageDraft` / `sendRichMessageDraft`.
pub fn new_draft_id() -> i64 {
    loop {
        let v = (rand::random::<u32>() & 0x7fff_ffff) as i64;
        if v != 0 {
            return v;
        }
    }
}

pub fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    (0..len)
        .map(|_| CHARS[rand::random::<u32>() as usize % CHARS.len()] as char)
        .collect()
}

/// Truncate to at most `max` characters, appending an ellipsis when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Sanitize a filename coming from an external system.
pub fn safe_filename(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.chars().take(120).collect()
    }
}

/// Best-effort conversion of a Python `repr` dict/list string into JSON.
/// DocsGPT tool results are Python reprs; this handles the common shapes.
pub fn python_repr_to_json(input: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
        return Some(v);
    }
    let mut out = String::with_capacity(input.len() + 8);
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut in_str: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match in_str {
            Some(q) => {
                if c == '\\' && i + 1 < chars.len() {
                    let n = chars[i + 1];
                    if n == q {
                        if q == '\'' {
                            out.push('\'');
                        } else {
                            out.push_str("\\\"");
                        }
                    } else {
                        out.push(c);
                        out.push(n);
                    }
                    i += 2;
                    continue;
                }
                if c == q {
                    out.push('"');
                    in_str = None;
                } else if c == '"' && q == '\'' {
                    out.push_str("\\\"");
                } else if c == '\n' {
                    out.push_str("\\n");
                } else {
                    out.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    in_str = Some(c);
                    out.push('"');
                } else if c.is_alphabetic() {
                    let start = i;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    out.push_str(match word.as_str() {
                        "True" => "true",
                        "False" => "false",
                        "None" => "null",
                        other => other,
                    });
                    continue;
                } else {
                    out.push(c);
                }
            }
        }
        i += 1;
    }
    serde_json::from_str(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_python_repr() {
        let v = python_repr_to_json("{'status': 'ok', 'stdout_tail': '1\\n', 'artifacts': [{'artifact_id': 'a', 'size': 18, 'ok': True, 'x': None}]}").unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["artifacts"][0]["artifact_id"], "a");
        assert_eq!(v["artifacts"][0]["ok"], true);
        assert!(v["artifacts"][0]["x"].is_null());
    }

    #[test]
    fn handles_quotes_inside() {
        let v = python_repr_to_json(r#"{'msg': "it's fine", 'q': 'say "hi"'}"#).unwrap();
        assert_eq!(v["msg"], "it's fine");
        assert_eq!(v["q"], "say \"hi\"");
    }

    #[test]
    fn truncates() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello world", 6), "hello…");
    }
}
