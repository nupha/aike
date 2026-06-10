//! Parallel inference engine for LLM.
//!
//! This module provides a multi-slot inference engine that can process
//! multiple tasks concurrently on a single LlamaContext, with support for
//! prefix caching, grammar-constrained generation, and streaming output.

mod cache;
mod context;
mod output;
pub(crate) mod serve;
mod slot;
mod task;
mod worker;

pub use {
    context::Engine,
    output::GeneratedContent,
    task::{EngineReply, EngineTask, GrammarConfig, StopReason, TaskConfig, Token},
};
