//! DocsGPT API client: streaming answers, attachments, speech, artifacts.

pub mod client;
pub mod sse;
pub mod types;

pub use client::{DocsGpt, StreamRequest};
pub use types::{ArtifactRef, Event, Source, ToolCall, ToolOutputs};
