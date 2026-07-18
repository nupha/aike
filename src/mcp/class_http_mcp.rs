// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, NativeObject, Runtime,
    ScopedContext, def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::{
    class_mcp::{JSBaseMcp, get_payload, js_str},
    service::{McpServicePayload, McpTransport, parse_mcp_define},
};

const FIELD_MOUNT_PATH: i16 = 12;

fn js_http_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let Some(cfg) = cc.arg(0).as_object().copied() else {
        ctx.throw_type_error("HttpMcp.define(config) — config must be an object");
        return Err(-1);
    };

    let transport = McpTransport::Http {
        mount_path: String::new(),
        update_tx: None,
    };

    let payload = parse_mcp_define(ctx, cfg, transport)?;

    JSHttpMcp::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

fn js_http_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error("HttpMcp cannot be constructed with `new`; use HttpMcp.define(config)");
    Err(-1)
}

/// http-only getter for `mountPath`.
fn js_http_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    match cc.magic() as i16 {
        FIELD_MOUNT_PATH => match &*p.transport.borrow() {
            McpTransport::Http { mount_path, .. } => js_str(ctx, mount_path.clone()),
            _ => Ok(JSValue::UNDEFINED),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

const HTTP_MCP_STATIC: &[FieldDef] = &[FieldDef::FUNC("define", 1, js_http_define, 0)];
const HTTP_MCP_PROTO: &[FieldDef] = &[FieldDef::GETSET(
    "mountPath",
    Some(js_http_get),
    None,
    FIELD_MOUNT_PATH,
)];

def_native_object!(
    "HttpMcp",
    JSHttpMcp,
    McpServicePayload,
    HTTP_MCP_PROTO,
    Some( ClassCtorDef::new(js_http_constructor, 1, true).entries(HTTP_MCP_STATIC)),
    extends: JSBaseMcp::class_id,
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpMcp::register(rt)
}
