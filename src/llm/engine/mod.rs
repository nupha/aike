// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Parallel inference engine for LLM.
//!
//! This module provides a multi-slot inference engine that can process
//! multiple tasks concurrently on a single LlamaContext, with support for
//! prefix caching, grammar-constrained generation, and streaming output.
//!
//! Only compiled when feature "llama_cpp" is enabled.

#![cfg(feature = "llama_cpp")]

mod caching;
mod context;
pub(crate) mod serve;
mod slot;
mod task;
mod worker;

mod output;

pub use {
    context::Engine,
    task::{EngineReply, EngineTask, GrammarConfig, StopReason, TaskConfig, Token},
};

pub use output::GeneratedContent;
