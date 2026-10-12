// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

mod server;

pub(crate) use server::{
    IncomingRequest, JSHttpRequest, JSHttpResponse, JSHttpResponseSse, JSHttpServer,
    JSWebSocketStream,
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
    const ALL: [Method; 7] = [
        Self::GET,
        Self::POST,
        Self::PUT,
        Self::PATCH,
        Self::DELETE,
        Self::HEAD,
        Self::OPTIONS,
    ];

    /// Parse an uppercase method name (route removal, axum request lines).
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.as_str() == s)
    }

    /// Method from its route-registration magic number.
    pub(crate) fn from_magic(m: u8) -> Option<Self> {
        Self::ALL.get(m as usize).copied()
    }

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
    ctx.use_classes(&[
        JSHttpServer::class_id(),
        JSHttpRequest::class_id(),
        JSHttpResponse::class_id(),
        JSHttpResponseSse::class_id(),
        JSWebSocketStream::class_id(),
    ])?;

    ctx.create_module("http", MOD_HTTP_ENTRIES)?;

    Ok(())
}
