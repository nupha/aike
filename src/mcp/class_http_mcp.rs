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
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, NativeObject, Object, Runtime,
    ScopedContext, def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::{
    class_mcp::{
        JSBaseMcp, get_payload, js_str,
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

const HTTP_MCP_STATIC_ENTRIES: &[FieldDef] = &[FieldDef::FUNC("define", 3, js_http_define, 0)];

const HTTP_MCP_CTOR: ClassCtorDef =
    ClassCtorDef::new(js_http_constructor, 1, true).entries(HTTP_MCP_STATIC_ENTRIES);

// Instance prototype — the http-only surface (shared getters/methods are
// inherited from `BaseMcp`, see class_mcp.rs).
const HTTP_MCP_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("mountPath", Some(js_http_get), None, FIELD_MOUNT_PATH),
];

def_native_object!(
    "HttpMcp",
    JSHttpMcp,
    McpServicePayload,
    HTTP_MCP_PROTO,
    Some(HTTP_MCP_CTOR),
    extends: JSBaseMcp::class_id,
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpMcp::register(rt)
}
