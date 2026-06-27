// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

// JS binding for HttpServer.
//
// HttpServer is a JS-side server instance with full lifecycle:
// create → register routes → listen → shutdown.
// Rust-side state is carried by HttpServerPayload.
// JS routes are delegated to axum via crossbeam channel + handler_index lookup.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::{FromRequestParts as _, RawPathParams, Request},
    response::{IntoResponse, Response as AxumResponse},
    routing::{MethodRouter, delete, get, head, options, patch, post, put},
};
use crossbeam::channel;
use linkme::distributed_slice;
use tokio::sync::oneshot;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, Object,
    PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, http::mcp::build_axum_mcp_route};

#[cfg(feature = "llama_cpp")]
use crate::llm::{
    JSEngine,
    engine::{Engine, serve::engine_openai_router},
};

use super::{
    Method,
    mcp::{McpEndpoint, McpSchemas, McpToolCallRequest, NativeMcpToolFn, handle_mcp_tool_call},
    request::{HttpRequestPayload, JSHttpRequest},
    response::{HttpResponseMessage, HttpResponsePayload, JSHttpResponse, get_res_payload},
    server::spawn_http_server,
};

/// Unified request message sent from axum delegate to the JS server job.
pub(crate) enum IncomingRequest {
    Http(HttpRequest),
    McpToolCall(McpToolCallRequest),
}

/// HTTP route request (existing logic, fields unchanged).
pub(super) struct HttpRequest {
    handler_index: usize,
    method: Method,
    path: String,
    query_string: String,
    params: HashMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    response_tx: oneshot::Sender<HttpResponseMessage>,
}

pub(super) struct HttpServerPayload {
    js_routes: Vec<(Method, String, Object)>,
    mcp_endpoints: Vec<McpEndpoint>,

    #[cfg(feature = "llama_cpp")]
    openai: Option<(Arc<std::sync::Mutex<Engine>>, String)>,

    shutdown_tx: Option<oneshot::Sender<()>>,
    is_running: Arc<AtomicBool>,
    frozen: bool,
}

impl HttpServerPayload {
    fn add_js_route(
        &mut self,
        method: Method,
        path: String,
        handler: Object,
    ) -> Result<(), &'static str> {
        if self.frozen {
            return Err("cannot add route while server is running");
        }
        // Replace existing entry for the same (method, path) to avoid duplicates
        if let Some(entry) = self
            .js_routes
            .iter_mut()
            .find(|(m, p, _)| m == &method && p == &path)
        {
            entry.2 = handler;
        } else {
            self.js_routes.push((method, path, handler));
        }
        Ok(())
    }

    fn remove_route(&mut self, method: Method, path: &str) -> Result<bool, &'static str> {
        if self.frozen {
            return Err("cannot remove route while server is running");
        }
        let before = self.js_routes.len();
        self.js_routes
            .retain(|(m, p, _)| !(m == &method && p == path));
        Ok(self.js_routes.len() < before)
    }

    /// Register a rust MCP tool on an existing MCP endpoint.
    ///
    /// Must be called BEFORE `listen()` because native handler closures are
    /// captured at router-build time. The schema is added to the shared table
    /// so `tools/list` and `tools/call` can find it.
    #[allow(dead_code)]
    pub(crate) fn register_mcp_tool(
        &mut self,
        mount_path: &str,
        name: &str,
        schema: serde_json::Value,
        handler: NativeMcpToolFn,
    ) -> Result<(), &'static str> {
        let ep = self
            .mcp_endpoints
            .iter_mut()
            .find(|e| e.mount_path == mount_path)
            .ok_or("no MCP endpoint at given mount path")?;

        // Add native handler
        let native_idx = ep.handlers.len();
        ep.handlers
            .push((name.to_string(), schema.clone(), handler));

        // Build rmcp::model::Tool from the schema
        let description = schema
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let input_schema = schema
            .get("inputSchema")
            .cloned()
            .unwrap_or(serde_json::json!({"type": "object"}));
        let input_schema_obj: serde_json::Map<String, serde_json::Value> = match input_schema {
            serde_json::Value::Object(m) => m,
            _ => serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let tool = rmcp::model::Tool::new(
            name.to_string(),
            description.to_string(),
            Arc::new(input_schema_obj.into()),
        );

        // Update shared schemas
        {
            let mut guard = ep.shared.write().map_err(|_| "schema lock poisoned")?;
            let schema_idx = guard.tools.len();
            guard.tools.push(tool);
            guard.name_to_index.insert(name.to_string(), schema_idx);
            guard.native_map.insert(name.to_string(), native_idx);
        }

        Ok(())
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for (_, _, handler) in &self.js_routes {
            handler.trace(gcx);
        }
        for ep in &self.mcp_endpoints {
            ep.gc_trace(gcx);
        }
    }
}

fn get_server_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a HttpServerPayload, i32> {
    val.as_native_object::<JSHttpServer>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("expect HttpServer instance");
            -1
        })
}

fn get_server_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HttpServerPayload, i32> {
    val.as_native_object_mut::<JSHttpServer>()
        .and_then(|o| o.payload_mut())
        .ok_or_else(|| {
            ctx.throw_type_error("expect HttpServer instance");
            -1
        })
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    JSHttpServer::new_instance(
        ctx,
        cc.this(),
        HttpServerPayload {
            js_routes: Vec::new(),
            mcp_endpoints: Vec::new(),

            #[cfg(feature = "llama_cpp")]
            openai: None,

            shutdown_tx: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frozen: false,
        },
    )
    .map(|v| v.into())
}

/// HttpServer.openai(engine, mountPath) — mount OpenAI-compatible routes.
///
/// `engine` must be an Engine instance (ownership is transferred).
/// `mountPath` must start with '/', e.g. "/v1".
/// Calling after listen() raises an error.
#[cfg(feature = "llama_cpp")]
fn js_openai(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // Validate engine argument
    let mut v = cc.arg(0);
    let engine = match v.as_native_object_mut::<JSEngine>() {
        Some(e) => match e.take_payload() {
            Some(payload) => *payload,
            None => {
                ctx.throw_type_error("Engine object has already been detached");
                return Err(-1);
            }
        },
        None => {
            ctx.throw_type_error("engine must be an Engine instance");
            return Err(-1);
        }
    };

    // Validate mountPath argument
    let mount_path = {
        let s = cc.arg(1).to_string(ctx).map_err(|_| {
            ctx.throw_type_error("mountPath must be a string");
            -1
        })?;
        let text = s.content().encode_utf8(false).to_string();
        if !text.starts_with('/') {
            ctx.throw_type_error("mountPath must start with '/'");
            return Err(-1);
        }
        text
    };

    let p = get_server_payload_mut(ctx, &mut this)?;
    if p.frozen {
        ctx.throw_type_error("cannot call openai() after listen()");
        return Err(-1);
    }
    p.openai = Some((engine, mount_path));

    Ok(this)
}

fn js_route_add(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let method = match cc.magic() as u8 {
        0 => Method::GET,
        1 => Method::POST,
        2 => Method::PUT,
        3 => Method::PATCH,
        4 => Method::DELETE,
        5 => Method::HEAD,
        6 => Method::OPTIONS,
        _ => {
            ctx.throw_internal_error("unknown method magic");
            return Err(-1);
        }
    };
    let path_str = cc.arg(0).to_string(ctx)?;
    let path = path_str.content().encode_utf8(false).to_string();
    let handler = cc.arg(1).into_callable(ctx)?;

    let p = get_server_payload_mut(ctx, &mut this)?;
    if let Err(e) = p.add_js_route(method, path, handler) {
        ctx.throw_type_error(e);
        return Err(-1);
    }
    Ok(this)
}

/// Convert DSL `args` object to standard JSON Schema `inputSchema`.
///
/// Input: `{ fieldName: { desc, type?, required?, enum?, default? } }`
/// Output: `{ type: "object", properties: { ... }, required: [...] }`
pub(crate) fn build_input_schema_from_args(
    ctx: &mut ScopedContext,
    mut args_obj: Object,
) -> Result<Option<serde_json::Value>, i32> {
    let pks = args_obj
        .get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK)?;

    if pks.is_empty() {
        return Ok(None);
    }

    let mut properties = serde_json::Map::new();
    let mut required: Vec<serde_json::Value> = Vec::new();

    for pk in pks.iter() {
        let field_name = match pk.atom.to_value(ctx).and_then(|v| v.to_string(ctx)) {
            Ok(s) => s.content().encode_utf8(false).to_string(),
            Err(_) => continue,
        };

        if field_name.is_empty() {
            continue;
        }

        let field_val = args_obj.get_property(ctx, pk.atom)?;
        let field_obj = match field_val.as_object() {
            Some(o) => o.clone(),
            None => continue,
        };

        let mut prop = serde_json::Map::new();

        // description
        if let Some(desc_val) = field_obj.get_property_str(ctx, "desc").ok() {
            if let Ok(s) = desc_val.to_string(ctx) {
                prop.insert(
                    "description".into(),
                    serde_json::Value::String(s.content().encode_utf8(false).to_string()),
                );
            }
        }

        // type (default: "string")
        let type_str = field_obj
            .get_property_str(ctx, "type")
            .ok()
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_else(|| "string".into());

        // enum
        let has_enum = field_obj
            .get_property_str(ctx, "enum")
            .ok()
            .filter(|v| !v.is_undefined())
            .is_some();
        if has_enum {
            if let Ok(enum_val) = field_obj.get_property_str(ctx, "enum") {
                if !enum_val.is_undefined() {
                    if let Ok(enum_json) = enum_val.to_serde_json(ctx) {
                        if !enum_json.is_null() {
                            prop.insert("enum".into(), enum_json);
                        }
                    }
                }
            }
        } else if type_str == "array" {
            prop.insert("type".into(), serde_json::Value::String("array".into()));

            // Parse items schema (defaults to { type: "string" })
            let mut items = serde_json::Map::new();
            if let Some(items_obj) = field_obj
                .get_property_str(ctx, "items")
                .ok()
                .filter(|v| !v.is_undefined())
                .and_then(|v| v.as_object().cloned())
            {
                let item_type = items_obj
                    .get_property_str(ctx, "type")
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.content().encode_utf8(false).to_string())
                    .unwrap_or_else(|| "string".into());
                items.insert("type".into(), serde_json::Value::String(item_type));

                if let Some(desc) = items_obj
                    .get_property_str(ctx, "desc")
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.content().encode_utf8(false).to_string())
                {
                    items.insert("description".into(), serde_json::Value::String(desc));
                }

                if let Some(enum_val) = items_obj
                    .get_property_str(ctx, "enum")
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.to_serde_json(ctx).ok())
                    .filter(|v| !v.is_null())
                {
                    items.insert("enum".into(), enum_val);
                }
            } else {
                items.insert("type".into(), serde_json::Value::String("string".into()));
            }
            prop.insert("items".into(), serde_json::Value::Object(items));
        } else {
            prop.insert("type".into(), serde_json::Value::String(type_str));
        }

        // default
        if let Some(default_val) = field_obj.get_property_str(ctx, "default").ok() {
            if let Ok(default_json) = default_val.to_serde_json(ctx) {
                if !default_json.is_null() {
                    prop.insert("default".into(), default_json);
                }
            }
        }

        // required
        if let Ok(req_val) = field_obj.get_property_str(ctx, "required") {
            if let Ok(req_json) = req_val.to_serde_json(ctx) {
                if req_json.as_bool() == Some(true) {
                    required.push(serde_json::Value::String(field_name.clone()));
                }
            }
        }

        properties.insert(field_name, serde_json::Value::Object(prop));
    }

    if properties.is_empty() {
        return Ok(None);
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), serde_json::Value::String("object".into()));
    schema.insert("properties".into(), serde_json::Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), serde_json::Value::Array(required));
    }

    Ok(Some(serde_json::Value::Object(schema)))
}

/// Parse tools from either array format (McpToolDefinition[]) or
/// object-keyed DSL format (ToolsMap).
///
/// Returns (rmcp Tool vec, name→index map, JS handler vec).
pub(crate) fn parse_mcp_tools(
    ctx: &mut ScopedContext,
    tools_val: &JSValue,
) -> Result<(Vec<rmcp::model::Tool>, HashMap<String, usize>, Vec<Object>), i32> {
    if let Some(o) = tools_val.as_object().copied() {
        if o.is_array(ctx).map_err(|_| {
            ctx.throw_type_error("tools is_array check failed");
            -1
        })? {
            parse_mcp_tools_array(ctx, o)
        } else {
            parse_mcp_tools_object(ctx, o)
        }
    } else {
        ctx.throw_type_error("tools must be an array or object");
        Err(-1)
    }
}

/// Parse tools in array format (McpToolDefinition[]).
///
/// Each element is standard MCP schema: `{ name, description?, inputSchema?, handler }`.
/// Extracts handler for JS call table, builds rmcp::model::Tool for protocol.
pub(crate) fn parse_mcp_tools_array(
    ctx: &mut ScopedContext,
    tools_arr: Object,
) -> Result<(Vec<rmcp::model::Tool>, HashMap<String, usize>, Vec<Object>), i32> {
    // Get array length via the standard JS `length` property
    let len = tools_arr
        .get_property_str(ctx, "length")
        .and_then(|v| v.to_number(ctx))
        .map(|n| n.to_i32())
        .map_err(|_| {
            ctx.throw_type_error("tools array must have a valid length");
            -1
        })? as usize;

    let mut tools: Vec<rmcp::model::Tool> = Vec::with_capacity(len);
    let mut name_to_index: HashMap<String, usize> = HashMap::new();
    let mut js_handlers: Vec<Object> = Vec::with_capacity(len);

    for i in 0..len {
        let elem = tools_arr.get_property_u32(ctx, i as u32)?;
        let tool_obj = elem
            .as_object()
            .ok_or_else(|| {
                ctx.throw_type_error("each tool must be an object");
                -1
            })?
            .clone();

        // name (required by MCP spec)
        let name = tool_obj
            .get_property_str(ctx, "name")
            .and_then(|v| v.to_string(ctx))
            .map(|s| s.content().encode_utf8(false).to_string())
            .map_err(|_| {
                ctx.throw_type_error("tool.name is required");
                -1
            })?;

        // description
        let description = tool_obj
            .get_property_str(ctx, "description")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();

        // inputSchema (required for array format — standard MCP Schema)
        let input_schema_json = tool_obj
            .get_property_str(ctx, "inputSchema")
            .and_then(|v| {
                v.as_object().copied().ok_or_else(|| {
                    ctx.throw_type_error(&format!("tool '{}' inputSchema must be an object", name));
                    -1
                })
            })
            .and_then(|o| o.to_serde_json(ctx))?;

        // handler (required) — stored in JS call table
        let handler = tool_obj
            .get_property_str(ctx, "handler")
            .and_then(|v| v.into_callable(ctx))
            .map_err(|_| {
                ctx.throw_type_error(&format!("tool '{}' handler must be a function", name));
                -1
            })?;

        let input_schema_obj: serde_json::Map<String, serde_json::Value> = match input_schema_json {
            serde_json::Value::Object(m) => m,
            _ => serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let tool =
            rmcp::model::Tool::new(name.clone(), description, Arc::new(input_schema_obj.into()));

        let idx = tools.len();
        tools.push(tool);
        name_to_index.insert(name, idx);
        js_handlers.push(handler);
    }

    Ok((tools, name_to_index, js_handlers))
}

/// Parse tools in object-keyed DSL format.
///
/// Keys are tool names, values are `{ desc, handler, args }`.
/// This mode does NOT accept `inputSchema` — use array format for that.
pub(crate) fn parse_mcp_tools_object(
    ctx: &mut ScopedContext,
    mut tools_obj: Object,
) -> Result<(Vec<rmcp::model::Tool>, HashMap<String, usize>, Vec<Object>), i32> {
    let pks = tools_obj
        .get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK)?;

    let mut tools: Vec<rmcp::model::Tool> = Vec::new();
    let mut name_to_index: HashMap<String, usize> = HashMap::new();
    let mut js_handlers: Vec<Object> = Vec::new();

    for pk in pks.iter() {
        let name = pk
            .atom
            .to_value(ctx)
            .and_then(|v| v.to_string(ctx))
            .map(|s| s.content().encode_utf8(false).to_string())
            .map_err(|_| {
                ctx.throw_type_error("tool name must be a string");
                -1
            })?;

        if name.is_empty() {
            continue;
        }

        let tool_obj = tools_obj
            .get_property(ctx, pk.atom)?
            .as_object()
            .ok_or_else(|| {
                ctx.throw_type_error(&format!("tool '{}' must be an object", name));
                -1
            })?
            .clone();

        // desc → description
        let description = tool_obj
            .get_property_str(ctx, "desc")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();

        // handler (required)
        let handler = tool_obj
            .get_property_str(ctx, "handler")
            .and_then(|v| v.into_callable(ctx))
            .map_err(|_| {
                ctx.throw_type_error(&format!("tool '{}' handler must be a function", name));
                -1
            })?;

        // Build inputSchema from args DSL (only mode for object-keyed format)
        let input_schema_json = if let Some(args_val) = tool_obj.get_property_str(ctx, "args").ok()
        {
            if !args_val.is_undefined() {
                if let Some(args_obj) = args_val.as_object() {
                    let args_cloned = args_obj.clone();
                    build_input_schema_from_args(ctx, args_cloned)?
                        .unwrap_or(serde_json::json!({"type": "object"}))
                } else {
                    serde_json::json!({"type": "object"})
                }
            } else {
                serde_json::json!({"type": "object"})
            }
        } else {
            serde_json::json!({"type": "object"})
        };

        let input_schema_obj: serde_json::Map<String, serde_json::Value> = match input_schema_json {
            serde_json::Value::Object(m) => m,
            _ => serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let tool =
            rmcp::model::Tool::new(name.clone(), description, Arc::new(input_schema_obj.into()));

        let idx = tools.len();
        tools.push(tool);
        name_to_index.insert(name, idx);
        js_handlers.push(handler);
    }

    Ok((tools, name_to_index, js_handlers))
}

/// HttpServer.mcp(mountPath, options) — register MCP server with tools.
///
/// `mountPath` must start with '/', e.g. "/mcp".
/// `options` is `{ serverName, version, tools: [{ name, description, inputSchema, handler }] }`.
/// Protocol methods (initialize, notifications/initialized, tools/list, tools/call)
/// are auto-generated in Rust. Returns `this` for chain-style calls.
fn js_mcp(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // arg(0): mountPath
    let mount_path = {
        let s = cc.arg(0).to_string(ctx).map_err(|_| {
            ctx.throw_type_error("mountPath must be a string");
            -1
        })?;
        let text = s.content().encode_utf8(false).to_string();
        if !text.starts_with('/') {
            ctx.throw_type_error("mountPath must start with '/'");
            return Err(-1);
        }
        text
    };

    // arg(1): options object
    let opts = cc
        .arg(1)
        .as_object()
        .ok_or_else(|| {
            ctx.throw_type_error("options must be an object");
            -1
        })?
        .clone();

    let server_name = opts
        .get_property_str(ctx, "serverName")
        .and_then(|v| v.to_string(ctx))
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|_| "mcp-server".into());

    let server_version = opts
        .get_property_str(ctx, "version")
        .and_then(|v| v.to_string(ctx))
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|_| "1.0.0".into());

    // tools (array or object-keyed format)
    let tools_val = opts.get_property_str(ctx, "tools").map_err(|_| {
        ctx.throw_type_error("options.tools is required");
        -1
    })?;

    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    let p = get_server_payload_mut(ctx, &mut this)?;
    if p.frozen {
        ctx.throw_type_error("cannot add mcp after listen()");
        return Err(-1);
    }

    p.mcp_endpoints.push(McpEndpoint {
        mount_path,
        server_name,
        server_version,
        shared: Arc::new(std::sync::RwLock::new(McpSchemas {
            tools,
            name_to_index,
            native_map: HashMap::new(),
        })),
        js_handlers,
        handlers: Vec::new(),
        update_tx: None,
    });

    Ok(this)
}

/// HttpServer.updateMcpTools(mountPath, tools) — hot-reload MCP tools at runtime.
///
/// Replaces all JS tools (native tools are preserved). Next request uses new handlers.
fn js_update_tools(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // arg(0): mountPath
    let mount_path = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .map_err(|_| {
            ctx.throw_type_error("mountPath must be a string");
            -1
        })?;

    // arg(1): tools (array or object-keyed format)
    let tools_val = cc.arg(1);

    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    // Find endpoint and update
    let p = get_server_payload_mut(ctx, &mut this)?;
    let ep = p
        .mcp_endpoints
        .iter_mut()
        .find(|e| e.mount_path == mount_path)
        .ok_or_else(|| {
            ctx.throw_type_error(&format!("no MCP endpoint at '{}'", mount_path));
            -1
        })?;

    // Update shared schemas (write lock)
    {
        let mut guard = ep.shared.write().unwrap();
        // Preserve native tool entries (names that are in native_map)
        let native_names: Vec<String> = guard.native_map.keys().cloned().collect();
        let mut new_tools = tools;
        let mut new_name_to_index = name_to_index;
        let mut new_native_map = HashMap::new();

        // Re-add native tool schemas at the end
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

    // Update JS handlers on endpoint (for GC tracing)
    ep.js_handlers = js_handlers.clone();

    // Send new handler table to the poller via update channel
    if let Some(ref utx) = ep.update_tx {
        let _ = utx.send(js_handlers);
    }

    Ok(this)
}

fn js_remove_route(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let method = match &*cc.arg(0).to_string(ctx)?.content().encode_utf8(false) {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        _ => {
            ctx.throw_type_error("unknown HTTP method");
            return Err(-1);
        }
    };
    let path_str = cc.arg(1).to_string(ctx)?;
    let path = path_str.content().encode_utf8(false).to_string();

    get_server_payload_mut(ctx, &mut this)?
        .remove_route(method, &path)
        .map_err(|e| {
            ctx.throw_type_error(e);
            -1
        })
        .map(JSValue::Bool)
}

/// Convert Express-style `:param` path segments to axum-style `{param}`.
fn express_to_axum_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' {
            let mut param = String::new();
            let mut pushed = false;
            for ch in chars.by_ref() {
                if ch.is_alphanumeric() || ch == '_' {
                    param.push(ch);
                } else {
                    result.push('{');
                    result.push_str(&param);
                    result.push('}');
                    result.push(ch);
                    pushed = true;
                    break;
                }
            }
            if !param.is_empty() && !pushed {
                result.push('{');
                result.push_str(&param);
                result.push('}');
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Validate axum path — reject segments that mix `{param}` with literal text.
/// This mirrors matchit's internal validation to avoid panics from router.route().
fn validate_axum_path(path: &str) -> Result<(), String> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(format!(
            "Invalid route \"{path}\": path must start with '/'"
        ));
    }
    for segment in path.split('/') {
        // Check for mixed param + literal: segment like "{taskId}restore"
        if let Some(open) = segment.find('{') {
            // param-like segment — ensure no literal text before '{'
            if open > 0 {
                return Err(format!(
                    "Invalid route \"{path}\": Only one parameter is allowed per path segment"
                ));
            }
            if let Some(close) = segment.rfind('}') {
                // literal text after '}'
                if close + 1 < segment.len() {
                    return Err(format!(
                        "Invalid route \"{path}\": Only one parameter is allowed per path segment"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Create a (path, MethodRouter) pair that delegates to a JS handler via channel.
///
/// The closure captures only `handler_index: usize` and `Sender` — both Send + Sync.
/// JS Object stays in the JS thread handler lookup table.
fn delegate_js_route(
    method: Method,
    path: String,
    handler_index: usize,
    request_tx: channel::Sender<IncomingRequest>,
) -> (String, MethodRouter<()>) {
    let axum_path = express_to_axum_path(&path);
    let tx = request_tx;

    let handler_fn = move |req: Request| {
        let tx = tx.clone();

        async move {
            // Split request to access extensions (axum stores parsed path params there)
            let (mut parts, body) = req.into_parts();
            let (res_tx, res_rx) = oneshot::channel();
            let uri = parts.uri.clone();
            let method = match parts.method.as_str() {
                "GET" => Method::GET,
                "POST" => Method::POST,
                "PUT" => Method::PUT,
                "PATCH" => Method::PATCH,
                "DELETE" => Method::DELETE,
                "HEAD" => Method::HEAD,
                "OPTIONS" => Method::OPTIONS,
                _ => {
                    return (
                        axum::http::StatusCode::METHOD_NOT_ALLOWED,
                        "method not allowed",
                    )
                        .into_response();
                }
            };
            let path = uri.path().to_string();
            let query_string = uri.query().unwrap_or("").to_string();
            let headers: Vec<(String, String)> = parts
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            // Extract path params from axum's already-parsed route match
            let params = match RawPathParams::from_request_parts(&mut parts, &()).await {
                Ok(raw) => raw
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<HashMap<_, _>>(),
                _ => HashMap::new(),
            };

            // Reconstruct request for body consumption
            let req = Request::from_parts(parts, body);
            let body = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
                Ok(b) => b.to_vec(),
                Err(_) => {
                    return (axum::http::StatusCode::BAD_REQUEST, "Failed to read body")
                        .into_response();
                }
            };

            if tx
                .send(IncomingRequest::Http(HttpRequest {
                    handler_index,
                    method,
                    path,
                    query_string,
                    params,
                    headers,
                    body,
                    response_tx: res_tx,
                }))
                .is_err()
            {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Server is shutting down",
                )
                    .into_response();
            }

            match res_rx.await {
                Ok(msg) => {
                    let status = axum::http::StatusCode::from_u16(msg.status)
                        .unwrap_or(axum::http::StatusCode::OK);
                    let mut response = AxumResponse::new(Body::from(msg.body.unwrap_or_default()));
                    *response.status_mut() = status;
                    for (k, v) in &msg.headers {
                        if let (Ok(name), Ok(val)) = (
                            axum::http::HeaderName::from_bytes(k.as_bytes()),
                            axum::http::HeaderValue::from_str(v),
                        ) {
                            response.headers_mut().insert(name, val);
                        }
                    }
                    response
                }
                Err(_) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                )
                    .into_response(),
            }
        }
    };

    // Wrap with method-specific router so multiple routes at the same path
    // (e.g. GET and PUT /api/users/:id) can coexist without fallback conflicts.
    let handler = match method {
        Method::GET => get(handler_fn),
        Method::POST => post(handler_fn),
        Method::PUT => put(handler_fn),
        Method::PATCH => patch(handler_fn),
        Method::DELETE => delete(handler_fn),
        Method::HEAD => head(handler_fn),
        Method::OPTIONS => options(handler_fn),
    };
    (axum_path, handler)
}

fn js_handler_then(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_reject = cc.magic() == 1;

    if is_reject {
        let reason = cc.arg(0);
        if !reason.is_undefined()
            && let Ok(s) = reason.to_string(ctx)
        {
            eprintln!("{}", s.content().encode_utf8(false));
        }
    }

    let res_val = cc.data(0);
    let p = get_res_payload(ctx, &res_val).unwrap();

    if !p.ended {
        p.status = 204;
        p.send_response();
    }

    Ok(JSValue::UNDEFINED)
}

fn js_listen(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // Check if already running
    if get_server_payload(ctx, &this)?.shutdown_tx.is_some() {
        ctx.throw_type_error("http server already running");
        return Err(-1);
    }

    // Parse options
    let mut port: u16 = 8080;
    let mut host: String = "0.0.0.0".into();

    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
            if let Ok(v) = opts.get_property_str(ctx, "port")
                && let Ok(n) = v.to_number(ctx)
                && let IntOrFloat::Int64(n) = n
            {
                port = n as u16;
            }

            if let Ok(v) = opts.get_property_str(ctx, "host")
                && let Ok(s) = v.to_string(ctx)
            {
                host = s.content().encode_utf8(false).to_string();
            }
        } else {
            ctx.throw_type_error_not_an_object();
            return Err(-1);
        }
    }

    // Build router, channel, and handler tables in one scope
    let (router, js_rx_opt, http_handler_table, mcp_handler_tables, mcp_update_rxs) = {
        let data = get_server_payload_mut(ctx, &mut this)?;
        data.frozen = true;

        // Base router with optional OpenAI engine routes
        let mut router: Router = Router::new();
        #[cfg(feature = "llama_cpp")]
        if let Some((ref eng, ref mount_path)) = data.openai {
            let engine_router = engine_openai_router(eng.clone());
            router = router.nest(mount_path, engine_router);
        }

        let has_js = !data.js_routes.is_empty() || !data.mcp_endpoints.is_empty();
        if !has_js {
            (router, None, Vec::new(), Vec::new(), Vec::new())
        } else {
            let (js_tx, js_rx) = channel::unbounded::<IncomingRequest>();

            // --- HTTP routes ---
            let http_table: Vec<Object> = data.js_routes.iter().map(|(_, _, h)| *h).collect();
            for (i, (method, path, _)) in data.js_routes.iter().enumerate() {
                let (axum_path, handler) =
                    delegate_js_route(*method, path.clone(), i, js_tx.clone());
                if let Err(e) = validate_axum_path(&axum_path) {
                    ctx.throw_type_error(&e);
                    return Err(-1);
                }
                router = router.route(&axum_path, handler);
            }

            // --- MCP routes: rmcp-based, protocol handled by rmcp crate ---
            let mut mcp_tables: Vec<Vec<Object>> = Vec::new();
            let mut mcp_update_rxs: Vec<channel::Receiver<Vec<Object>>> = Vec::new();
            for (i, ep) in data.mcp_endpoints.iter_mut().enumerate() {
                // Build axum route via rmcp's StreamableHttpService
                let mcp_axum_router = build_axum_mcp_route(ep, js_tx.clone(), i);
                router = router.merge(mcp_axum_router);
                // JS handler table for poller-side dispatch
                mcp_tables.push(ep.js_handlers.clone());
                // Create update channel for hot-reload of tools
                let (utx, urx) = channel::unbounded::<Vec<Object>>();
                ep.update_tx = Some(utx);
                mcp_update_rxs.push(urx);
            }

            (router, Some(js_rx), http_table, mcp_tables, mcp_update_rxs)
        }
    };

    // Spawn the unified axum service
    let handle = match spawn_http_server(router, host, port) {
        Ok(h) => h,
        Err(e) => {
            ctx.throw_internal_error(&e);
            return Err(-1);
        }
    };

    // Store handle — after spawn, is_running is true
    {
        let p = get_server_payload_mut(ctx, &mut this)?;
        p.shutdown_tx = Some(handle.shutdown_tx);
        p.is_running = handle.is_running.clone();
    }

    // Clone is_running for the exit promise before handle.is_running gets moved
    let running_flag = handle.is_running.clone();

    // Start unified server job on JS thread (handles HTTP and MCP tool calls)
    if let Some(js_rx) = js_rx_opt {
        js_start_server_job(
            ctx,
            js_rx,
            http_handler_table,
            mcp_handler_tables,
            mcp_update_rxs,
            handle.is_running,
        );
    }

    // Create exit promise — resolves when the server fully stops
    let (mut exit_promise, exit_tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;

    // Spawn tokio task that polls is_running and resolves the exit promise
    tokio::spawn(async move {
        while running_flag.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = PollSender::from(exit_tx).send_async(Ok(())).await;
    });

    // Create handle object with exit (readonly Promise) and shutdown()
    let mut handle_obj = ctx.create_base_object().map_err(|_| {
        ctx.throw_internal_error("failed to create handle object");
        -1
    })?;

    // Define a hidden reference to the HttpServer on the exit promise.
    // This keeps the server alive as long as the promise is referenced
    // (e.g. after startServer returns handle.exit and handle goes out of scope).
    exit_promise.define_property_value_str(
        ctx,
        "_server",
        this.into(),
        0, // hidden: not configurable, not writable, not enumerable
    )?;

    // prop: exit — readonly (no WRITABLE flag)
    handle_obj.define_property_value_str(
        ctx,
        "exit",
        exit_promise.into(),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    // method: shutdown — function that sends stop signal
    let shutdown_fn = ctx.create_native_function_with_data(
        Callback::Func(js_handle_shutdown),
        "shutdown",
        0,
        0,
        [this].into(),
    )?;
    handle_obj.define_property_value_str(
        ctx,
        "shutdown",
        shutdown_fn.into(),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    Ok(handle_obj.into())
}

/// Start the unified server job via queue_job — polls crossbeam channel on JS thread.
/// Handles HTTP and MCP tool call requests through the same channel.
fn js_start_server_job(
    ctx: &mut ScopedContext,
    rx: channel::Receiver<IncomingRequest>,
    http_handler_table: Vec<Object>,
    mcp_handler_tables: Vec<Vec<Object>>,
    mcp_update_rxs: Vec<channel::Receiver<Vec<Object>>>,
    is_running: Arc<AtomicBool>,
) {
    let is_running2 = is_running.clone();
    // Wrap MCP tables in RefCell for hot-reload support
    let mcp_tables = RefCell::new(mcp_handler_tables);
    crate::helpers::start_channel_poller(
        ctx,
        "http_mcp",
        rx,
        move |incoming: IncomingRequest, cx| {
            // Check for tool table updates before processing
            for (i, urx) in mcp_update_rxs.iter().enumerate() {
                if let Ok(new_table) = urx.try_recv() {
                    mcp_tables.borrow_mut()[i] = new_table;
                }
            }
            match incoming {
                IncomingRequest::Http(req) => {
                    dispatch_http_request(cx, req, &http_handler_table);
                }
                IncomingRequest::McpToolCall(req) => {
                    handle_mcp_tool_call(cx, req, &mcp_tables.borrow());
                }
            }
            true // keep polling
        },
        move || is_running2.load(Ordering::Acquire),
        || {},
    );
}

/// Dispatch an HTTP request to the appropriate JS handler.
fn dispatch_http_request(cx: &mut ScopedContext, incoming: HttpRequest, handler_table: &[Object]) {
    let handler = &handler_table[incoming.handler_index];
    let query = parse_query_string(&incoming.query_string);

    let req_val: JSValue = match JSHttpRequest::new_instance(
        cx,
        JSValue::UNDEFINED,
        HttpRequestPayload {
            method: incoming.method,
            path: incoming.path.clone(),
            params: incoming.params.clone(),
            query,
            headers: incoming.headers.iter().cloned().collect(),
            body: Some(incoming.body),
        },
    ) {
        Ok(v) => v.into(),
        Err(_) => return,
    };

    let res_val: JSValue = match JSHttpResponse::new_instance(
        cx,
        JSValue::UNDEFINED,
        HttpResponsePayload {
            status: 200,
            headers: Vec::new(),
            body: None,
            ended: false,
            response_tx: Some(incoming.response_tx),
        },
    ) {
        Ok(v) => v.into(),
        Err(_) => return,
    };

    // Call JS handler
    match handler.call(cx, JSValue::UNDEFINED, &[req_val, res_val]) {
        Ok(val) => {
            if let Ok(mut promise) = Promise::try_from(val) {
                // Async handler — attach then/catch
                let ok_fn = match cx.create_native_function_with_data(
                    Callback::Func(js_handler_then),
                    "",
                    0,
                    0,
                    [res_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => return,
                };
                let fail_fn = match cx.create_native_function_with_data(
                    Callback::Func(js_handler_then),
                    "",
                    1,
                    1,
                    [res_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => return,
                };

                if promise.then(cx, ok_fn, fail_fn).is_err() {
                    let p = get_res_payload(cx, &val).unwrap();
                    p.status = 500;
                    p.body = Some(String::from("Internal Server Error"));
                    p.send_response();
                }
            } else {
                // Sync handler
                if let Ok(p) = get_res_payload(cx, &res_val)
                    && !p.ended
                {
                    p.status = 204;
                    p.send_response();
                }
            }
        }
        Err(_) => {
            let exc = cx.take_exception();
            let msg = exc.to_text(cx);
            if !msg.message.is_empty() {
                eprintln!("{}", msg.message);
            }
            if let Ok(p) = get_res_payload(cx, &res_val)
                && !p.ended
            {
                p.status = 500;
                p.body = Some(String::from("Internal Server Error"));
                p.send_response();
            }
        }
    }
}

fn js_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    let (shutdown_tx, is_running) = {
        let p = get_server_payload_mut(ctx, &mut this)?;
        if p.shutdown_tx.is_none() {
            return Ok(JSValue::UNDEFINED);
        }

        let tx = p.shutdown_tx.take();
        let r = p.is_running.clone();
        p.frozen = false;
        (tx, r)
    };

    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }

    // Create promise that resolves when server fully stops
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;

    tokio::spawn(async move {
        while is_running.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = PollSender::from(tx).send_async(Ok(())).await;
    });

    Ok(promise.into())
}

/// Handle's shutdown — sends stop signal only, returns immediately.
/// The caller should await handle.exit to wait for the server to fully stop.
fn js_handle_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut server_val = cc.data(0);
    let shutdown_tx = {
        let p = get_server_payload_mut(ctx, &mut server_val)?;
        if p.shutdown_tx.is_none() {
            return Ok(JSValue::UNDEFINED);
        }
        let tx = p.shutdown_tx.take();
        p.frozen = false;
        tx
    };
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }
    Ok(JSValue::UNDEFINED)
}

fn js_is_running(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_server_payload(ctx, &this)?;
    Ok(JSValue::from(data.shutdown_tx.is_some()))
}

fn parse_query_string(qs: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if qs.is_empty() {
        return map;
    }
    for pair in qs.split('&') {
        if let Some(pos) = pair.find('=') {
            map.insert(url_decode(&pair[..pos]), url_decode(&pair[pos + 1..]));
        } else {
            map.insert(url_decode(pair), String::new());
        }
    }
    map
}

fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(hex_val);
            let lo = chars.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                result.push((h << 4 | l) as char);
            } else {
                result.push('%');
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

const SERVER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("get", 2, js_route_add, 0),
    FieldDef::FUNC("post", 2, js_route_add, 1),
    FieldDef::FUNC("put", 2, js_route_add, 2),
    FieldDef::FUNC("patch", 2, js_route_add, 3),
    FieldDef::FUNC("delete", 2, js_route_add, 4),
    FieldDef::FUNC("head", 2, js_route_add, 5),
    FieldDef::FUNC("options", 2, js_route_add, 6),
    FieldDef::FUNC("mcp", 2, js_mcp, 0),
    FieldDef::FUNC("updateMcpTools", 2, js_update_tools, 0),
    FieldDef::FUNC("removeRoute", 2, js_remove_route, 0),
    #[cfg(feature = "llama_cpp")]
    FieldDef::FUNC("openai", 2, js_openai, 0),
    FieldDef::GETSET("isRunning", Some(js_is_running), None, 0),
    FieldDef::FUNC("listen", 1, js_listen, 0),
    FieldDef::FUNC("shutdown", 0, js_shutdown, 0),
];

def_native_object!(
    "HttpServer",
    JSHttpServer,
    HttpServerPayload,
    SERVER_PROTO,
    Some(ClassCtorDef::new(js_constructor, 0, false)),
    on_payload_gc: HttpServerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_server(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpServer::register(rt)
}
