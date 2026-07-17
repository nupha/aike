// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{Arc, RwLock},
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::{
    class_stdio_mcp::{
        get_payload, get_payload_mut, js_add_tool, js_initialize_info, js_instructions, js_name,
        js_str, js_tools_list, js_version,
    },
    service::{McpSchemas, McpServicePayload, McpTransport, parse_mcp_tools},
};

const FIELD_MOUNT_PATH: i16 = 12;

/// `HttpMcp.define(name, tools, options)` — static factory.
///
/// `name` is the advertised server name (required, same position as
/// `StdioMcp.define`). `tools` is the required second argument (object-keyed
/// DSL or array). `options` carries version / instructions (everything except
/// `tools`). The compiled schemas + handlers are written into the payload at
/// define() time. The mount path is NOT set here — it is assigned by
/// `HttpServer.mcp(mountPath, server)` at mount time.
fn js_http_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // name — required first argument (advertised server name).
    let name = cc.arg(0).to_string(ctx).map_err(|_| {
        ctx.throw_type_error("HttpMcp.define name must be a string");
        -1
    })?;
    let name = name.content().encode_utf8(false).to_string();

    // tools — required second argument (object-keyed DSL or array).
    let tools_val = cc.arg(1);
    if tools_val.as_object().is_none() {
        ctx.throw_type_error("`tools` must be an array or object");
        return Err(-1);
    }

    let mut payload = McpServicePayload::new_http();
    payload.server_name = name;

    // Optional third `options` object (version / instructions).
    let opt = cc.arg(2);
    if !opt.is_undefined() {
        let Some(obj) = opt.as_object().copied() else {
            ctx.throw_type_error("HttpMcp.define options must be an object");
            return Err(-1);
        };
        apply_http_options(ctx, &mut payload, obj)?;
    }

    // Shared parser: write compiled schemas + js_handlers into the payload.
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;
    payload.schemas = Arc::new(RwLock::new(McpSchemas {
        tools,
        name_to_index,
        native_map: HashMap::new(),
    }));
    payload.js_handlers = RefCell::new(js_handlers);

    JSHttpMcp::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

/// Apply the `HttpMcp.define(name, tools, options)` options object onto a
/// fresh payload. Covers version / instructions (serverName is positional).
fn apply_http_options(
    ctx: &mut ScopedContext,
    p: &mut McpServicePayload,
    opt: Object,
) -> Result<(), i32> {
    if let Some(v) = opt
        .get_property_str(ctx, "version")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *p.version.borrow_mut() = Cow::Owned(s);
    }
    if let Some(v) = opt
        .get_property_str(ctx, "instructions")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *p.instructions.borrow_mut() = Cow::Owned(s);
    }

    Ok(())
}

fn js_http_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error(
        "HttpMcp cannot be constructed with `new`; use HttpMcp.define(name, tools, options)",
    );
    Err(-1)
}

/// http-only getter for `mountPath`.
fn js_http_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    match cc.magic() as i16 {
        FIELD_MOUNT_PATH => match &p.transport {
            McpTransport::Http { mount_path, .. } => js_str(ctx, mount_path.clone()),
            _ => Ok(JSValue::UNDEFINED),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

/// `HttpMcp.updateTools(tools)` — hot-reload: REPLACE all JS tools (native
/// tools are preserved). For an HTTP server that is already listening, the
/// new handler table is pushed through the hot-reload channel so it takes
/// effect on the next request.
fn js_http_update_tools(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_payload_mut(ctx, &mut this)?;

    // HttpMcp is always an Http transport, but guard for safety.
    match &p.transport {
        McpTransport::Http { .. } => {}
        _ => {
            ctx.throw_type_error("updateTools is only supported on an Http transport");
            return Err(-1);
        }
    }

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

    // Send new handler table to the poller via update channel.
    if let McpTransport::Http { update_tx, .. } = &p.transport {
        if let Some(utx) = update_tx {
            let _ = utx.send(js_handlers);
        }
    }

    Ok(this)
}

const HTTP_MCP_STATIC_ENTRIES: &[FieldDef] = &[FieldDef::FUNC("define", 3, js_http_define, 0)];

const HTTP_MCP_CTOR: ClassCtorDef =
    ClassCtorDef::new(js_http_constructor, 1, true).entries(HTTP_MCP_STATIC_ENTRIES);

const HTTP_MCP_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("initializeInfo", Some(js_initialize_info), None, 0),
    FieldDef::GETSET("toolsList", Some(js_tools_list), Some(js_tools_list), 0),
    FieldDef::GETSET("name", Some(js_name), None, 0),
    FieldDef::GETSET("version", Some(js_version), None, 0),
    FieldDef::GETSET("instructions", Some(js_instructions), None, 0),
    FieldDef::GETSET("mountPath", Some(js_http_get), None, FIELD_MOUNT_PATH),
    FieldDef::FUNC("addTool", 1, js_add_tool, 0),
    FieldDef::FUNC("updateTools", 1, js_http_update_tools, 0),
];

def_native_object!(
    "HttpMcp",
    JSHttpMcp,
    McpServicePayload,
    HTTP_MCP_PROTO,
    Some(HTTP_MCP_CTOR),
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpMcp::register(rt)
}
