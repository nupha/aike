//! OpenAI Responses API handler.
//! Only compiled when feature "llama_cpp" is enabled.

#![cfg(feature = "llama_cpp")]

pub(crate) mod convert;
pub(crate) mod stream;

mod handler;
pub use handler::handle_responses;
