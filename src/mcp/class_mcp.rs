// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::{
    class_http_mcp::JSHttpMcp,
    class_stdio_mcp::JSStdioMcp,
    service::{McpServicePayload, McpTransport, parse_mcp_tools},
};

/// Create a JS string value, taking ownership of `s` so no extra allocation
/// is needed when the caller already holds a `String`.
pub(crate) fn js_str(ctx: &mut ScopedContext, s: String) -> JSResult {
    Ok(ctx.create_string_from_utf8(s.into_bytes())?.into())
}

/// Resolve the shared `McpServicePayload` from either a `JSStdioMcp` or a
/// `JSHttpMcp` instance (both wrap the same payload type).
pub(crate) fn get_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a McpServicePayload, i32> {
    if let Some(o) = val.as_native_object::<JSStdioMcp>() {
        if let Some(p) = o.payload() {
            return Ok(p);
        }
    }
    if let Some(o) = val.as_native_object::<JSHttpMcp>() {
        if let Some(p) = o.payload() {
            return Ok(p);
        }
    }
    ctx.throw_type_error("not a Mcp server instance");
    Err(-1)
}

/// Mutable variant of `get_payload` — returns the payload for mutation
/// (e.g. dynamically adding tools).
pub(crate) fn get_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut McpServicePayload, i32> {
    // Peek the class via immutable borrows first (repeatable), then take a
    // single mutable borrow inside the matching branch. This avoids two
    // sequential `&mut` borrows of `val`, which the borrow checker rejects
    // (the `&'a mut` return would otherwise tie `val` as borrowed for `'a`
    // across the whole function).
    let is_stdio = val.as_native_object::<JSStdioMcp>().is_some();
    let is_http = val.as_native_object::<JSHttpMcp>().is_some();
    if is_stdio {
        return val
            .as_native_object_mut::<JSStdioMcp>()
            .and_then(|o| o.payload_mut())
            .ok_or_else(|| {
                ctx.throw_type_error("not a Mcp server instance");
                -1
            });
    }
    if is_http {
        return val
            .as_native_object_mut::<JSHttpMcp>()
            .and_then(|o| o.payload_mut())
            .ok_or_else(|| {
                ctx.throw_type_error("not a Mcp server instance");
                -1
            });
    }
    ctx.throw_type_error("not a Mcp server instance");
    Err(-1)
}

/// tools JSON-schema getter (mirrors `McpServerDef.toolSchema`). The setter is
/// a no-op (servers are built from the payload at define/run time).
pub(crate) fn js_tools_list(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    p.make_schemas_json(ctx).map(JSValue::from)
}

/// `initializeInfo` getter — returns the object advertised in the MCP
/// `initialize` handshake result. Built from `make_server_info` (== the
/// `InitializeResult` the server actually returns), then serialized to a JS
/// object via serde_json.
pub(crate) fn js_initialize_info(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    let info = p.make_server_info();
    let val = serde_json::to_value(info).map_err(|e| {
        ctx.throw_internal_error(&format!("server info serialize failed: {}", e));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

pub(crate) fn js_name(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.server_name.clone())
}

pub(crate) fn js_version(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.version.borrow().to_string())
}

pub(crate) fn js_instructions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.instructions.borrow().to_string())
}

/// Dynamically add JS tools after construction. Accepts the same `tools` DSL as
/// `define` (object-keyed map or `McpToolDefinition[]` array). For an HTTP server
/// that is already listening, the new handler table is pushed through the
/// hot-reload channel so it takes effect on the next request. For stdio, call
/// BEFORE `run()` (the server is built from the payload at run time).
pub(crate) fn js_add_tool(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_payload_mut(ctx, &mut this)?;

    let tools_val = cc.arg(0);
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    let new_handlers = p.register_js_tools(tools, name_to_index, js_handlers);

    // HTTP hot-reload: push the new handler table so the running poller picks it up.
    if let McpTransport::Http { update_tx, .. } = &p.transport {
        if let Some(tx) = update_tx {
            let _ = tx.send(new_handlers);
        }
    }

    Ok(this)
}

/// `updateTools(tools)` — hot-reload: REPLACE all JS tools (native tools are
/// preserved). Shared by stdio + http. For an HTTP server that is already
/// listening, the new handler table is pushed through the hot-reload channel
/// so it takes effect on the next request. For stdio, call BEFORE `run()`
/// (the server is built from the payload at run time).
pub(crate) fn js_update_tools(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_payload_mut(ctx, &mut this)?;

    // arg(0): tools (array or object-keyed format)
    let tools_val = cc.arg(0);
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    // Replace all JS tools, preserving native tool entries.
    {
        let mut guard = p.schemas.write().unwrap();
        let native_names: Vec<String> = guard.native_map.keys().cloned().collect();
        let mut new_tools = tools;
        let mut new_name_to_index = name_to_index;
        let mut new_native_map = HashMap::new();

        for name in &native_names {
            if let Some(&old_idx) = guard.name_to_index.get(name) {
                let new_idx = new_tools.len();
                new_tools.push(guard.tools[old_idx].clone());
                new_name_to_index.insert(name.clone(), new_idx);
                if let Some(&ni) = guard.native_map.get(name) {
                    new_native_map.insert(name.clone(), ni);
                }
            }
        }

        guard.tools = new_tools;
        guard.name_to_index = new_name_to_index;
        guard.native_map = new_native_map;
    }

    // Update JS handlers on payload (for GC tracing).
    p.js_handlers = RefCell::new(js_handlers.clone());

    // HTTP hot-reload: push the new handler table so the running poller picks it up.
    if let McpTransport::Http { update_tx, .. } = &p.transport {
        if let Some(utx) = update_tx {
            let _ = utx.send(js_handlers);
        }
    }

    Ok(this)
}

// ── JSBaseMcp ──
//
// Abstract base class holding the prototype methods/getters shared by
// `JSStdioMcp` and `JSHttpMcp` (both wrap the same `McpServicePayload`).
// `StdioMcp` extends this class via `extends:`; `HttpMcp` will follow later.
// Never constructed or exported to the global object directly.

fn js_base_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error(
        "BaseMcp is abstract and cannot be constructed directly; use StdioMcp.define / HttpMcp.define",
    );
    Err(-1)
}

// Instance prototype — the surface shared by every Mcp server (stdio + http).
const BASE_MCP_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("initializeInfo", Some(js_initialize_info), None, 0),
    FieldDef::GETSET("toolsList", Some(js_tools_list), Some(js_tools_list), 0),
    FieldDef::GETSET("name", Some(js_name), None, 0),
    FieldDef::GETSET("version", Some(js_version), None, 0),
    FieldDef::GETSET("instructions", Some(js_instructions), None, 0),
    FieldDef::FUNC("addTool", 1, js_add_tool, 0),
    FieldDef::FUNC("updateTools", 1, js_update_tools, 0),
];

def_native_object!(
    "BaseMcp",
    JSBaseMcp,
    McpServicePayload,
    BASE_MCP_PROTO,
    Some(ClassCtorDef::new(js_base_constructor, 0, false)),
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_base_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSBaseMcp::register(rt)
}
