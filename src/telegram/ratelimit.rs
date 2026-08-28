//! Pacing for Telegram limits: ~30 messages/second overall, ~1 message/second
//! per chat, and a tighter budget for draft updates.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A message that lands in the chat.
    Message,
    /// A draft update (`sendMessageDraft`).
    Draft,
    /// Chat actions, reactions, callback answers.
    Light,
}

pub struct RateLimiter {
    global: Mutex<Bucket>,
    per_chat: Mutex<HashMap<(i64, Kind), Instant>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
    rate: f64,
    burst: f64,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            global: Mutex::new(Bucket {
                tokens: 25.0,
                last: Instant::now(),
                rate: 25.0,
                burst: 25.0,
            }),
            per_chat: Mutex::new(HashMap::new()),
        }
    }

    /// `TG_RATE_LIMITS=off` disables pacing (tests, local Bot API servers).
    fn disabled() -> bool {
        std::env::var("TG_RATE_LIMITS")
            .map(|v| v == "off" || v == "0")
            .unwrap_or(false)
    }

    fn gap(kind: Kind) -> Duration {
        if Self::disabled() {
            return Duration::ZERO;
        }
        match kind {
            Kind::Message => Duration::from_millis(1000),
            Kind::Draft => Duration::from_millis(450),
            Kind::Light => Duration::from_millis(150),
        }
    }

    /// Wait until a call of `kind` to `chat_id` fits the budget.
    pub async fn acquire(&self, chat_id: i64, kind: Kind) {
        // Per-chat spacing.
        let wait = {
            let mut map = self.per_chat.lock().await;
            let now = Instant::now();
            let gap = Self::gap(kind);
            let next = map.get(&(chat_id, kind)).map(|t| *t + gap).unwrap_or(now);
            let slot = next.max(now);
            map.insert((chat_id, kind), slot);
            if map.len() > 10_000 {
                map.retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));
            }
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        if kind == Kind::Light || Self::disabled() {
            return;
        }
        // Global token bucket.
        loop {
            let sleep_for = {
                let mut b = self.global.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(b.last).as_secs_f64();
                b.tokens = (b.tokens + elapsed * b.rate).min(b.burst);
                b.last = now;
                if b.tokens >= 1.0 {
                    b.tokens -= 1.0;
                    None
                } else {
                    Some(Duration::from_secs_f64((1.0 - b.tokens) / b.rate))
                }
            };
            match sleep_for {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spaces_messages_per_chat() {
        let l = RateLimiter::new();
        let t0 = Instant::now();
        l.acquire(1, Kind::Message).await;
        l.acquire(1, Kind::Message).await;
        assert!(t0.elapsed() >= Duration::from_millis(900));
        let t1 = Instant::now();
        l.acquire(2, Kind::Message).await;
        assert!(t1.elapsed() < Duration::from_millis(200));
    }
}
