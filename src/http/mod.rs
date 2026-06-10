// HTTP module root.
//
// Submodules:
// - server:           Pure Rust axum service core (spawn_http_service)
// - class_http_server: JS binding (HttpServer class, delegate, Job)
// - request:          JSHttpRequest native class
// - response:         JSHttpResponse native class

pub(crate) mod server;

mod class_http_server;
mod request;
mod response;

pub(crate) use class_http_server::{JSHttpServer, init_mod_http};
pub(crate) use request::JSHttpRequest;
pub(crate) use response::JSHttpResponse;

/// HTTP method enum — shared across submodules.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum Method {
    GET = 0,
    POST = 1,
    PUT = 2,
    PATCH = 3,
    DELETE = 4,
    HEAD = 5,
    OPTIONS = 6,
}

impl Method {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::GET => "GET",
            Self::POST => "POST",
            Self::PUT => "PUT",
            Self::PATCH => "PATCH",
            Self::DELETE => "DELETE",
            Self::HEAD => "HEAD",
            Self::OPTIONS => "OPTIONS",
        }
    }
}

const MOD_HTTP_ENTRIES: &[FieldDef] = &[FieldDef::CLASS(
    "HttpServer",
    JSHttpServer::class_id,
    zuqe::JS_PROP_ENUMERABLE,
)];

pub fn init_mod_http(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("http", MOD_HTTP_ENTRIES)?;
    Ok(())
}
