// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_http_server;
mod class_request;
mod outbound_buffer;
mod response;
mod server;
mod ws;

pub(crate) use {
    class_http_server::{IncomingRequest, JSHttpServer},
    class_request::JSHttpRequest,
    response::{JSHttpResponse, JSHttpResponseSse},
    ws::JSWebSocketStream,
};

use zuqe::{FieldDef, NativeObject, ScopedContext};

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
    JSHttpServer::CLASS_NAME,
    JSHttpServer::class_id,
    zuqe::JS_PROP_ENUMERABLE,
)];

pub fn init_mod_http(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("http", MOD_HTTP_ENTRIES)?;
    Ok(())
}
