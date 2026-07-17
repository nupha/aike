// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_mcp_server_def;
mod class_stdio_mcp;
mod service;

pub(crate) use class_stdio_mcp::gc_trace_stdio_state;
pub(crate) use service::{
    McpEndpoint, McpSchemas, McpToolCallRequest, NativeMcpToolFn, build_axum_mcp_route,
    handle_mcp_tool_call,
};

use {class_mcp_server_def::JSMcpServerDef, class_stdio_mcp::JSStdioMcp};

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, Object, ScopedContext};

/// Shared payload types for the stdio / server-def MCP builders.
/// `class_mcp_stdio` (`StdioMcp`) and `class_mcp_server_def` (`McpServerDef`)
/// both store tools / env / args and reuse them via `parse_env_object` /
/// `dsl_to_json_schema`.
#[derive(Clone)]
pub(crate) enum EnvVal {
    Text(String),
    Func(Object),
}

#[derive(Clone)]
pub(crate) enum ArgsVal {
    Text(Vec<String>),
    Func(Object),
}

#[derive(Clone)]
pub(crate) struct McpToolDef {
    pub(crate) desc: String,
    pub(crate) handler: Object,
    pub(crate) args: Option<Object>,
}

pub(crate) fn init_mod_mcp(ctx: &mut ScopedContext) -> Result<(), i32> {
    const MOD_MCP_ENTRIES: &[FieldDef] = &[
        FieldDef::CLASS(
            JSMcpServerDef::CLASS_NAME,
            JSMcpServerDef::class_id,
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::CLASS(
            JSStdioMcp::CLASS_NAME,
            JSStdioMcp::class_id,
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::FUNC(
            "defineStdioMcp",
            2,
            class_mcp_server_def::js_define_mcp_stdio,
            0,
        ),
    ];

    ctx.use_classes(&[JSMcpServerDef::class_id(), JSStdioMcp::class_id()])?;
    ctx.create_module("mcp", MOD_MCP_ENTRIES)?;

    Ok(())
}
