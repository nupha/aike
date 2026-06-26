pub(crate) mod convert;
pub(crate) mod response_builder;
pub(crate) mod stream;

#[cfg(feature = "llama_cpp")]
mod handler;
#[cfg(feature = "llama_cpp")]
pub use handler::handle_responses;
