// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    pin::Pin,
    sync::{Arc, RwLock},
};

use crossbeam::channel;
use linkme::distributed_slice;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo, Tool};
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK,
    JSResult, JSValue, Object, Runtime, ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::{CommandArgs, EnvVal, class_http_mcp::JSHttpMcp, class_stdio_mcp::JSStdioMcp};

/// Native tool handler function type.
pub(crate) type NativeMcpToolFn = Arc<
    dyn Fn(
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>
        + Send
        + Sync,
>;

/// MCP schemas shared between tokio closures (Send + Sync).
/// Does NOT contain Object (which is !Send).
pub(crate) struct McpSchemas {
    /// All tool schemas (native + JS), as rmcp::model::Tool values
    pub tools: Vec<rmcp::model::Tool>,
    /// Tool name -> index in tools
    pub name_to_index: HashMap<String, usize>,
    /// Tool name -> index in native_handlers (only for native tools)
    pub native_map: HashMap<String, usize>,
}

/// Transport-specific difference — the only thing needing an enum.
///
/// `Stdio` carries the process-level run config (before_run / spawn_env /
/// spawn_args / shared / extra_files); `Http` carries the install / hot-reload
/// fields (mount_path / update_tx). See mcp-service.md §3.
pub(crate) enum McpTransport {
    /// Process-level run config (mcp-service.md §2.1): `before_run` = init hook
    /// run once before `serve`; `spawn_env` / `spawn_args` = env / CLI args
    /// forwarded to `Aike.spawn` when the framework launches the aike child
    /// process. The server side only exposes getters — the framework consumes
    /// them.
    Stdio {
        command: String,
        command_env: HashMap<String, EnvVal>,
        command_args: CommandArgs,
        before_run: Option<Object>,
        extra_files: Vec<String>,
    },
    /// http-only install / hot-reload fields.
    Http {
        mount_path: String,
        update_tx: Option<channel::Sender<Vec<Object>>>,
    },
}

/// Single payload held on the JS thread (contains `Object`, so `!Send`).
/// Both `JSStdioMcp` and `JSHttpMcp` wrap this; the transport enum encodes the
/// only real difference between the two server kinds.
pub(crate) struct McpServicePayload {
    pub(crate) server_name: String,
    pub(crate) version: RefCell<Cow<'static, str>>,
    pub(crate) description: RefCell<String>,
    pub(crate) instructions: RefCell<Cow<'static, str>>,

    /// tool schemas accessible from tokio (Send + Sync).
    pub(crate) schemas: Arc<RwLock<McpSchemas>>,

    /// JS handler objects — GC roots + dispatch targets for the poller.
    pub(crate) js_handlers: RefCell<Vec<Object>>,

    /// Native handlers (always empty in the JS path today; kept for future
    /// Rust-registered native tools). See mcp-service.md §8.
    pub(crate) handlers: Vec<(String, serde_json::Value, NativeMcpToolFn)>,

    /// Transport-specific difference.
    pub(crate) transport: RefCell<McpTransport>,
}

impl McpServicePayload {
    /// Initialize common fields; the transport is supplied by the caller.
    pub(super) fn new(server_name: String, description: String, transport: McpTransport) -> Self {
        Self {
            server_name,
            version: RefCell::new(Cow::Borrowed("1.0.0")),
            description: RefCell::new(description),
            instructions: RefCell::new(Cow::Borrowed("")),
            schemas: Arc::new(RwLock::new(McpSchemas {
                tools: Vec::new(),
                name_to_index: HashMap::new(),
                native_map: HashMap::new(),
            })),
            handlers: Vec::new(),
            js_handlers: RefCell::new(Vec::new()),
            transport: RefCell::new(transport),
        }
    }

    /// GC trace — keeps referenced JS objects (handlers, callbacks) alive.
    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        let t = self.transport.borrow();
        match &*t {
            McpTransport::Stdio {
                command_env: spawn_env,
                command_args: spawn_args,
                before_run,
                ..
            } => {
                for v in spawn_env.values() {
                    if let EnvVal::Func(f) = v {
                        f.trace(gcx);
                    }
                }
                if let CommandArgs::Func(f) = spawn_args {
                    f.trace(gcx);
                }
                if let Some(f) = before_run {
                    f.trace(gcx);
                }
            }
            McpTransport::Http { .. } => {}
        }
        for h in self.js_handlers.borrow().iter() {
            h.trace(gcx);
        }
    }

    /// Build the MCP `initialize` handshake result from this payload. Mirrors
    /// the `ServerInfo` (== `InitializeResult`) the live server advertises, so
    /// it stays in sync with the handshake.
    pub(crate) fn make_server_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();

        let mut si =
            Implementation::new(self.server_name.clone(), self.version.borrow().to_string());
        if !self.description.borrow().is_empty() {
            si = si.with_description(self.description.borrow().to_string());
        }

        let info = ServerInfo::new(capabilities)
            .with_server_info(si)
            .with_protocol_version(ProtocolVersion::V_2025_06_18);

        if self.instructions.borrow().is_empty() {
            info
        } else {
            info.with_instructions(self.instructions.borrow().to_string())
        }
    }

    /// Serialize the registered tools into a JS `Array` of JSON-schema objects.
    /// Each `Tool` is `#[derive(Serialize)]`, so it is converted via serde_json
    /// rather than hand-built.
    pub(crate) fn make_schemas_json(&self, ctx: &mut ScopedContext) -> Result<Array, i32> {
        let guard = self.schemas.read().map_err(|e| {
            ctx.throw_internal_error(&format!("schema lock poisoned: {}", e));
            -1
        })?;

        let mut lst = Vec::with_capacity(guard.tools.len());
        for tool in guard.tools.iter() {
            let val = serde_json::to_value(tool).map_err(|e| {
                ctx.throw_internal_error(&format!("tool serialize failed: {}", e));
                -1
            })?;
            let meta = JSValue::from_serde_json(ctx, &val).unwrap_or_else(|_| {
                ctx.create_base_object()
                    .map(|o| o.into())
                    .unwrap_or(JSValue::UNDEFINED)
            });
            lst.push(meta);
        }

        Array::from_values(ctx, lst)
    }

    /// Register a native (Rust) tool on this payload. The schema is added to
    /// the shared table so `tools/list` and `tools/call` can find it.
    pub(crate) fn register_native_tool(
        &mut self,
        name: &str,
        schema: serde_json::Value,
        handler: NativeMcpToolFn,
    ) -> Result<(), &'static str> {
        let native_idx = self.handlers.len();
        self.handlers
            .push((name.to_string(), schema.clone(), handler));

        // Build rmcp::model::Tool from the schema.
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
        let tool = Tool::new(
            name.to_string(),
            description.to_string(),
            Arc::new(input_schema_obj.into()),
        );

        let mut guard = self.schemas.write().map_err(|_| "schema lock poisoned")?;
        let schema_idx = guard.tools.len();
        guard.tools.push(tool);
        guard.name_to_index.insert(name.to_string(), schema_idx);
        guard.native_map.insert(name.to_string(), native_idx);

        Ok(())
    }

    /// Append JS tools (already parsed by `parse_mcp_tools`) to this payload.
    ///
    /// `name_to_index` from the parser is rebased onto the current
    /// `js_handlers` length so that `tools/call` dispatch
    /// (name_to_index -> mcp_tables -> js_handlers) stays consistent with any
    /// existing JS tools. Returns the new full `js_handlers` vec — for HTTP this
    /// is sent through `update_tx` so the running server hot-reloads.
    pub(crate) fn register_js_tools(
        &mut self,
        tools: Vec<Tool>,
        name_to_index: HashMap<String, usize>,
        js_handlers: Vec<Object>,
    ) -> Vec<Object> {
        let base = self.js_handlers.borrow().len();
        {
            let mut guard = self.schemas.write().unwrap();
            for (name, &local_idx) in &name_to_index {
                let global_idx = base + local_idx;
                guard.name_to_index.insert(name.clone(), global_idx);
                guard.tools.push(tools[local_idx].clone());
            }
        }
        self.js_handlers.borrow_mut().extend(js_handlers);
        self.js_handlers.borrow().clone()
    }
}

/// Create a JS string value, taking ownership of `s` so no extra allocation
/// is needed when the caller already holds a `String`.
pub(crate) fn js_str(ctx: &mut ScopedContext, s: String) -> JSResult {
    Ok(ctx.create_string_from_utf8(s.into_bytes())?.into())
}

pub(super) fn get_payload<'a>(
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

pub(super) fn get_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut McpServicePayload, i32> {
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

fn js_tools_list(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    p.make_schemas_json(ctx).map(JSValue::from)
}

fn js_initialize_info(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    let info = p.make_server_info();
    let val = serde_json::to_value(info).map_err(|e| {
        ctx.throw_internal_error(&format!("server info serialize failed: {}", e));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

fn js_name(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.server_name.clone())
}

fn js_version(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.version.borrow().to_string())
}

fn js_instructions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.instructions.borrow().to_string())
}

fn js_add_tool(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_payload_mut(ctx, &mut this)?;

    let tools_val = cc.arg(0);
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    let new_handlers = p.register_js_tools(tools, name_to_index, js_handlers);

    // HTTP hot-reload: push the new handler table so the running poller picks it up.
    if let McpTransport::Http { update_tx, .. } = &*p.transport.borrow() {
        if let Some(tx) = update_tx {
            let _ = tx.send(new_handlers);
        }
    }

    Ok(this)
}

fn js_update_tools(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
    if let McpTransport::Http { update_tx, .. } = &*p.transport.borrow() {
        if let Some(utx) = update_tx {
            let _ = utx.send(js_handlers);
        }
    }

    Ok(this)
}

fn js_base_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error(
        "BaseMcp is abstract and cannot be constructed directly; use StdioMcp.define / HttpMcp.define",
    );
    Err(-1)
}

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

// ---------------------------------------------------------------------------
// Unified tool DSL parser — shared by StdioMcp.define and HttpMcp.define.
// Returns (rmcp Tool vec, name→index map, JS handler vec). This is the single
// source of truth for both transports (mcp-service.md §4).
// ---------------------------------------------------------------------------

/// Convert DSL `args` object to standard JSON Schema `inputSchema`.
///
/// Input: `{ fieldName: { desc, type?, required?, enum?, default? } }`
/// Output: `{ type: "object", properties: { ... }, required: [...] }`
/// Returns `None` when there are no fields (so callers can fall back to a bare
/// `{ type: "object" }`). Used by `parse_mcp_tools` when building each tool's
/// `inputSchema` for the rmcp protocol layer.
pub(crate) fn build_input_schema_from_args(
    ctx: &mut ScopedContext,
    mut args_obj: Object,
) -> Result<Option<serde_json::Value>, i32> {
    let pks = args_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;

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

/// Shared config parsing for `StdioMcp.define` / `HttpMcp.define`.
///
/// Parses the common fields (`name` (required, non-empty), `version`,
/// `instructions`, `tools` (required, object)) from the single config object,
/// compiles the tool schemas + JS handlers, and returns a fully-initialized
/// `McpServicePayload` carrying `transport`. The caller then applies
/// transport-specific options (e.g. stdio's command/env/args).
///
/// Returns the constructed payload.
pub(crate) fn parse_mcp_define(
    ctx: &mut ScopedContext,
    cfg: Object,
    transport: McpTransport,
) -> Result<McpServicePayload, i32> {
    // name — required.
    let name_val = cfg.get_property_str(ctx, "name")?;
    if name_val.is_undefined() {
        ctx.throw_type_error("Mcp.define: `name` is required");
        return Err(-1);
    }
    let name = name_val
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    if name.is_empty() {
        ctx.throw_type_error("Mcp.define: `name` must not be empty");
        return Err(-1);
    }

    // description
    let desc = {
        let v = cfg.get_property_str(ctx, "description")?;
        if !v.is_undefined() {
            v.as_string()
                .map(|s| s.content().encode_utf8(false).to_string())
        } else {
            None
        }
    }
    .unwrap_or(String::new());

    // tools — required
    let tools_val = cfg.get_property_str(ctx, "tools")?;
    if tools_val.as_object().is_none() {
        ctx.throw_type_error("Mcp.define: `tools` must be an object");
        return Err(-1);
    }

    let mut payload = McpServicePayload::new(name, desc, transport);

    // version
    if let Some(v) = cfg
        .get_property_str(ctx, "version")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *payload.version.borrow_mut() = Cow::Owned(s);
    }

    // instructions
    if let Some(v) = cfg
        .get_property_str(ctx, "instructions")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *payload.instructions.borrow_mut() = Cow::Owned(s);
    }

    // Shared tool parser: write compiled schemas + js_handlers into the payload.
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;
    payload.schemas = std::sync::Arc::new(std::sync::RwLock::new(McpSchemas {
        tools,
        name_to_index,
        native_map: HashMap::new(),
    }));
    payload.js_handlers = RefCell::new(js_handlers);

    Ok(payload)
}

/// Parse tools from either array format (McpToolDefinition[]) or
/// object-keyed DSL format (ToolsMap).
///
/// Returns (rmcp Tool vec, name→index map, JS handler vec).
pub(crate) fn parse_mcp_tools(
    ctx: &mut ScopedContext,
    tools_val: &JSValue,
) -> Result<(Vec<Tool>, HashMap<String, usize>, Vec<Object>), i32> {
    let Some(o) = tools_val.as_object().copied() else {
        ctx.throw_type_error("tools must be an object");
        return Err(-1);
    };
    // Only the object-keyed DSL is supported; arrays are rejected.
    if o.is_array(ctx).map_err(|_| {
        ctx.throw_type_error("tools is_array check failed");
        -1
    })? {
        ctx.throw_type_error("tools must be an object, not an array");
        return Err(-1);
    }
    parse_mcp_tools_object(ctx, o)
}

/// Parse tools in object-keyed DSL format.
///
/// Keys are tool names, values are `{ desc, handler, args }`.
/// This mode does NOT accept `inputSchema` — use array format for that.
pub(crate) fn parse_mcp_tools_object(
    ctx: &mut ScopedContext,
    mut tools_obj: Object,
) -> Result<(Vec<Tool>, HashMap<String, usize>, Vec<Object>), i32> {
    let pks = tools_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;

    let mut tools: Vec<Tool> = Vec::new();
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

        let tool = Tool::new(name.clone(), description, Arc::new(input_schema_obj.into()));

        let idx = tools.len();
        tools.push(tool);
        name_to_index.insert(name, idx);
        js_handlers.push(handler);
    }

    Ok((tools, name_to_index, js_handlers))
}
