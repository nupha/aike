//! Parallel inference engine for LLM.
//!
//! This module provides a multi-slot inference engine that can process
//! multiple tasks concurrently on a single LlamaContext, with support for
//! prefix caching, grammar-constrained generation, and streaming output.

#[cfg(feature = "llama_cpp")]
mod caching;
#[cfg(feature = "llama_cpp")]
mod context;
#[cfg(feature = "llama_cpp")]
pub(crate) mod serve;
#[cfg(feature = "llama_cpp")]
mod slot;
#[cfg(feature = "llama_cpp")]
mod task;
#[cfg(feature = "llama_cpp")]
mod worker;

mod output;

#[cfg(feature = "llama_cpp")]
pub use {
    context::Engine,
    task::{EngineReply, EngineTask, GrammarConfig, StopReason, TaskConfig, Token},
};

pub use output::GeneratedContent;
