//! Telegram Bot API layer: typed client wrapper, raw calls for fields the
//! crate doesn't know yet, rendering, rate limiting and the update runtime.

pub mod api;
pub mod ratelimit;
pub mod raw;
pub mod render;
pub mod runtime;
pub mod webhook;

pub use api::Tg;
