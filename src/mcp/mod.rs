// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_http_mcp;
mod class_mcp;
mod class_stdio_mcp;
mod service;

pub(crate) use {
    class_http_mcp::JSHttpMcp,
    class_mcp::JSBaseMcp,
    class_stdio_mcp::{JSStdioMcp, gc_trace_stdio_state},
    service::{McpToolCallRequest, McpTransport, build_axum_mcp_route, handle_mcp_tool_call},
};

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, Object, ScopedContext};

#[derive(Clone)]
pub(super) enum EnvVal {
    Text(String),
    Func(Object),
}

#[derive(Clone)]
pub(super) enum ArgsVal {
    Text(Vec<String>),
    Func(Object),
}

pub(crate) fn init_mod_mcp(ctx: &mut ScopedContext) -> Result<(), i32> {
    const MOD_MCP_ENTRIES: &[FieldDef] = &[
        FieldDef::CLASS(
            JSStdioMcp::CLASS_NAME,
            JSStdioMcp::class_id,
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::CLASS(
            JSHttpMcp::CLASS_NAME,
            JSHttpMcp::class_id,
            JS_PROP_ENUMERABLE,
        ),
    ];

    ctx.use_classes(&[
        JSBaseMcp::class_id(),
        JSStdioMcp::class_id(),
        JSHttpMcp::class_id(),
    ])?;

    ctx.create_module("mcp", MOD_MCP_ENTRIES)?;

    Ok(())
}
