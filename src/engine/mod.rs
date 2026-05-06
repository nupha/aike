//! Parallel inference engine for LLM.
//!
//! This module provides a multi-slot inference engine that can process
//! multiple tasks concurrently on a single LlamaContext, with support for
//! prefix caching, grammar-constrained generation, and streaming output.

mod context;
mod slot;
mod task;
mod worker;

pub use {
    context::Engine,
    task::{EngineReply, GrammarConfig, OutputMode, StopReason, TaskConfig, Token},
};
