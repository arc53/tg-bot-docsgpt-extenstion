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

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub use docsgpt_bot::util::{safe_filename, truncate_chars};
