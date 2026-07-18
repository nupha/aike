// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    pin::Pin,
    sync::{Arc, RwLock},
};

use crossbeam::channel;
use linkme::distributed_slice;
use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::{
        CallToolResult, Content, CustomNotification, Implementation, ListToolsResult,
        ProtocolVersion, ServerCapabilities, ServerInfo, ServerNotification, Tool,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    },
};
use zuqe::{
    Array, CallContext, Callback, ClassID, FieldDef, JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK,
    JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, http::IncomingRequest};

use super::{CommandArgs, EnvVal};

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
        spawn_env: RefCell<HashMap<String, EnvVal>>,
        spawn_args: RefCell<CommandArgs>,
        before_run: RefCell<Option<Object>>,
        is_shared: Cell<bool>,
        extra_files: RefCell<Vec<String>>,
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
    pub(crate) instructions: RefCell<Cow<'static, str>>,

    /// tool schemas accessible from tokio (Send + Sync).
    pub(crate) schemas: Arc<RwLock<McpSchemas>>,

    /// JS handler objects — GC roots + dispatch targets for the poller.
    pub(crate) js_handlers: RefCell<Vec<Object>>,

    /// Native handlers (always empty in the JS path today; kept for future
    /// Rust-registered native tools). See mcp-service.md §8.
    pub(crate) handlers: Vec<(String, serde_json::Value, NativeMcpToolFn)>,

    /// Transport-specific difference.
    pub(crate) transport: McpTransport,
}

impl McpServicePayload {
    pub(crate) fn new_stdio(server_name: String) -> Self {
        Self {
            server_name,
            version: RefCell::new(Cow::Borrowed("1.0.0")),
            instructions: RefCell::new(Cow::Borrowed("")),
            schemas: Arc::new(RwLock::new(McpSchemas {
                tools: Vec::new(),
                name_to_index: HashMap::new(),
                native_map: HashMap::new(),
            })),
            js_handlers: RefCell::new(Vec::new()),
            handlers: Vec::new(),
            transport: McpTransport::Stdio {
                spawn_env: RefCell::new(HashMap::new()),
                spawn_args: RefCell::new(CommandArgs::Empty),
                before_run: RefCell::new(None),
                is_shared: Cell::new(false),
                extra_files: RefCell::new(Vec::new()),
            },
        }
    }

    pub(crate) fn new_http() -> Self {
        Self {
            server_name: "mcp-server".to_string(),
            version: RefCell::new(Cow::Borrowed("1.0.0")),
            instructions: RefCell::new(Cow::Borrowed("")),
            schemas: Arc::new(RwLock::new(McpSchemas {
                tools: Vec::new(),
                name_to_index: HashMap::new(),
                native_map: HashMap::new(),
            })),
            js_handlers: RefCell::new(Vec::new()),
            handlers: Vec::new(),
            transport: McpTransport::Http {
                mount_path: String::new(),
                update_tx: None,
            },
        }
    }

    /// GC trace — keeps referenced JS objects (handlers, callbacks) alive.
    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        match &self.transport {
            McpTransport::Stdio {
                spawn_env,
                spawn_args,
                before_run,
                ..
            } => {
                for v in spawn_env.borrow().values() {
                    if let EnvVal::Func(f) = v {
                        f.trace(gcx);
                    }
                }
                if let CommandArgs::Func(f) = &*spawn_args.borrow() {
                    f.trace(gcx);
                }
                if let Some(f) = &*before_run.borrow() {
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
        let info = ServerInfo::new(capabilities)
            .with_server_info(Implementation::new(
                self.server_name.clone(),
                self.version.borrow().to_string(),
            ))
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

/// MCP tool call request sent via crossbeam channel to the JS thread.
/// Uses mpsc so the JS handler can send multiple chunks (SSE events) before
/// the final result. The final message has `final_chunk: true`.
pub(crate) struct McpToolCallRequest {
    pub(crate) endpoint_index: usize,
    pub(crate) method_index: usize,
    pub(crate) params: serde_json::Value,
    pub(crate) response_tx: tokio::sync::mpsc::Sender<McpChunkMessage>,
}

/// A simple error for MCP tool call dispatch.
pub(crate) struct McpChunkError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

/// A single chunk sent from JS tool handler to Rust MCP service.
/// Either an intermediate SSE notification event or the final CallToolResult.
pub(crate) struct McpChunkMessage {
    /// If true, this is the final message — the stream ends after this.
    pub(crate) final_chunk: bool,
    /// For intermediate chunks: the JSON-RPC notification payload (as JSON string).
    /// For the final chunk: the full CallToolResult JSON (as before).
    pub(crate) result_json: Option<String>,
    /// Error (only meaningful for the final chunk).
    pub(crate) error: Option<McpChunkError>,
}

/// MCP Bridge: wraps mpsc::Sender<McpChunkMessage> for streaming tool results
pub(crate) struct McpBridgePayload {
    tx: Option<tokio::sync::mpsc::Sender<McpChunkMessage>>,
}

impl McpBridgePayload {
    /// Send an intermediate chunk (SSE notification event).
    fn send_chunk(&mut self, json: String) {
        if let Some(ref tx) = self.tx {
            let _ = tx.try_send(McpChunkMessage {
                final_chunk: false,
                result_json: Some(json),
                error: None,
            });
        }
    }

    /// Send the final result and close the channel.
    fn send_final(&mut self, msg: McpChunkMessage) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.try_send(msg);
        }
    }
}

fn get_mcp_bridge_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut McpBridgePayload, i32> {
    val.as_native_object_mut::<JSMcpBridge>()
        .and_then(|o| o.payload_mut())
        .ok_or_else(|| {
            ctx.throw_internal_error("McpBridge was detached");
            -1
        })
}

const MCP_BRIDGE_PROTO: &[FieldDef] = &[];

def_native_object!(
    "McpBridge",
    JSMcpBridge,
    McpBridgePayload,
    MCP_BRIDGE_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_mcp_bridge(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSMcpBridge::register(rt)
}

/// Create a JSMcpBridge instance wrapping the given mpsc sender.
fn create_mcp_bridge(
    ctx: &mut ScopedContext,
    tx: tokio::sync::mpsc::Sender<McpChunkMessage>,
) -> Result<JSValue, i32> {
    JSMcpBridge::new_instance(ctx, JSValue::UNDEFINED, McpBridgePayload { tx: Some(tx) })
        .map(|v| v.into())
}

/// The MCP service that bridges rmcp protocol to our native + JS tool handlers.
///
/// Implements `ServerHandler` manually (not via `#[tool_router]`) because tools
/// are dynamic — JS tools are defined at runtime via the JS API.
#[derive(Clone)]
pub(crate) struct McpService {
    /// Shared schemas for tools/list
    shared: Arc<RwLock<McpSchemas>>,
    /// Native handlers indexed by name
    native_handlers: Arc<HashMap<String, NativeMcpToolFn>>,
    /// Crossbeam channel sender for dispatching JS tool calls
    js_tx: channel::Sender<IncomingRequest>,
    /// Index of this endpoint in the global mcp_tables
    endpoint_index: usize,

    /// Server metadata
    server_name: String,
    server_version: String,
    server_instructions: String,
}

impl McpService {
    pub(crate) fn new(
        shared: Arc<RwLock<McpSchemas>>,
        handlers: Vec<(String, serde_json::Value, NativeMcpToolFn)>,
        js_tx: channel::Sender<IncomingRequest>,
        endpoint_index: usize,
        server_name: String,
        server_version: String,
        server_instructions: String,
    ) -> Self {
        let native_handlers: HashMap<String, NativeMcpToolFn> = handlers
            .into_iter()
            .map(|(name, _schema, f)| (name, f))
            .collect();

        Self {
            shared,
            native_handlers: Arc::new(native_handlers),
            js_tx,
            endpoint_index,
            server_name,
            server_version,
            server_instructions,
        }
    }

    /// Convert a `McpServicePayload` into an `McpService` (mcp-service.md §3).
    /// Native handlers are always empty in the JS path today.
    pub(crate) fn from_payload(
        p: &McpServicePayload,
        js_tx: channel::Sender<IncomingRequest>,
        endpoint_index: usize,
    ) -> Self {
        Self::new(
            p.schemas.clone(),
            p.handlers.clone(),
            js_tx,
            endpoint_index,
            p.server_name.clone(),
            p.version.borrow().to_string(),
            p.instructions.borrow().to_string(),
        )
    }
}

impl ServerHandler for McpService {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();

        let info = ServerInfo::new(capabilities)
            .with_server_info(Implementation::new(
                self.server_name.clone(),
                self.server_version.clone(),
            ))
            .with_protocol_version(ProtocolVersion::V_2025_03_26);

        if !self.server_instructions.is_empty() {
            info.with_instructions(self.server_instructions.clone())
        } else {
            info
        }
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            let guard = self.shared.read().map_err(|e| {
                McpError::internal_error(format!("schema lock poisoned: {}", e), None)
            })?;
            Ok(ListToolsResult::with_all_items(guard.tools.clone()))
        }
    }

    fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let tool_name = request.name.to_string();
        let arguments: serde_json::Value = request
            .arguments
            .map(|m| serde_json::Value::Object(m))
            .unwrap_or(serde_json::Value::Null);

        async move {
            // Check native handlers first
            if let Some(handler) = self.native_handlers.get(&tool_name) {
                return match handler(arguments).await {
                    Ok(result) => {
                        let content = Content::json(result)
                            .unwrap_or_else(|_| Content::text("Failed to serialize result"));
                        Ok(CallToolResult::success(vec![content]))
                    }
                    Err(msg) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: {}",
                        msg
                    ))])),
                };
            }

            // JS tool — dispatch through mpsc channel for streaming SSE support.
            let method_index = {
                let guard = self.shared.read().map_err(|e| {
                    McpError::internal_error(format!("schema lock poisoned: {}", e), None)
                })?;
                let Some(&idx) = guard.name_to_index.get(&tool_name) else {
                    return Err(McpError::invalid_params(
                        format!("Unknown tool: {}", tool_name),
                        None,
                    ));
                };
                idx
            };

            // Use mpsc (buffer 32) so the JS handler can send multiple chunks
            // before the final result. Intermediate chunks are forwarded as
            // SSE notification events to the MCP client.
            let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel::<McpChunkMessage>(32);
            let _send_result = self
                .js_tx
                .send(IncomingRequest::McpToolCall(McpToolCallRequest {
                    endpoint_index: self.endpoint_index,
                    method_index,
                    params: arguments,
                    response_tx: resp_tx,
                }));

            // Send a "started" notification before JS handler runs.
            {
                let start_notif = CustomNotification::new(
                    "notifications/tool-chunk",
                    Some(serde_json::json!({"status": "started", "tool": tool_name})),
                );
                let _ = context
                    .peer
                    .send_notification(ServerNotification::CustomNotification(start_notif))
                    .await;
            }

            // Read chunks from the JS handler.
            // Intermediate chunks → SSE notification events.
            // Final chunk → CallToolResult response.
            loop {
                match resp_rx.recv().await {
                    Some(chunk) => {
                        if chunk.final_chunk {
                            if let Some(err) = chunk.error {
                                return Ok(CallToolResult::error(vec![Content::text(format!(
                                    "Error: {}",
                                    err.message
                                ))]));
                            }
                            let result_json = chunk.result_json.as_deref().unwrap_or("null");
                            return match serde_json::from_str::<CallToolResult>(result_json) {
                                Ok(ctr) => Ok(ctr),
                                Err(_e) => Ok(CallToolResult::success(vec![Content::text(
                                    result_json.to_string(),
                                )])),
                            };
                        } else {
                            // Intermediate chunk: send as custom notification (SSE event).
                            if let Some(ref json) = chunk.result_json {
                                let notif = CustomNotification::new(
                                    "notifications/tool-chunk",
                                    Some(
                                        serde_json::from_str::<serde_json::Value>(json)
                                            .unwrap_or(serde_json::Value::String(json.clone())),
                                    ),
                                );
                                let _ = context
                                    .peer
                                    .send_notification(ServerNotification::CustomNotification(
                                        notif,
                                    ))
                                    .await;
                            }
                        }
                    }
                    None => {
                        eprintln!(
                            "[MCP] handler channel closed unexpectedly for tool: {}",
                            tool_name
                        );
                        return Err(McpError::internal_error(
                            "handler channel closed unexpectedly",
                            None,
                        ));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MCP transport — Streamable HTTP (stateful, session-based, SSE)
// ---------------------------------------------------------------------------

/// Build an axum Router for the MCP service via Streamable HTTP.
///
/// The returned Router can be merged into an existing axum app via
/// `router.merge(mcp_router)`.
pub(crate) fn build_axum_mcp_route(
    p: &McpServicePayload,
    js_tx: channel::Sender<IncomingRequest>,
    endpoint_index: usize,
) -> axum::Router {
    let service = McpService::from_payload(p, js_tx, endpoint_index);
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(true)
        .disable_allowed_hosts();
    let session_manager = Arc::new(LocalSessionManager::default());

    let mcp_svc = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(service.clone()),
        session_manager,
        config,
    );

    let mount_path = match &p.transport {
        McpTransport::Http { mount_path, .. } => mount_path.clone(),
        _ => String::new(),
    };
    axum::Router::new().route(&mount_path, axum::routing::any_service(mcp_svc))
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

/// Parse tools from either array format (McpToolDefinition[]) or
/// object-keyed DSL format (ToolsMap).
///
/// Returns (rmcp Tool vec, name→index map, JS handler vec).
pub(crate) fn parse_mcp_tools(
    ctx: &mut ScopedContext,
    tools_val: &JSValue,
) -> Result<(Vec<Tool>, HashMap<String, usize>, Vec<Object>), i32> {
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
) -> Result<(Vec<Tool>, HashMap<String, usize>, Vec<Object>), i32> {
    // Get array length via the standard JS `length` property
    let len = tools_arr
        .get_property_str(ctx, "length")
        .and_then(|v| v.to_number(ctx))
        .map(|n| n.to_i32())
        .map_err(|_| {
            ctx.throw_type_error("tools array must have a valid length");
            -1
        })? as usize;

    let mut tools: Vec<Tool> = Vec::with_capacity(len);
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

        let tool = Tool::new(name.clone(), description, Arc::new(input_schema_obj.into()));

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

// ---------------------------------------------------------------------------
// Poller-side handlers (run on JS thread)
// ---------------------------------------------------------------------------

/// Handle an MCP tool call request on the JS thread.
/// Looks up the handler from the JS-thread handler table by method_index.
/// Supports streaming: the JS handler can send intermediate chunks via the
/// bridge before sending the final result.
pub(crate) fn handle_mcp_tool_call(
    cx: &mut ScopedContext,
    req: McpToolCallRequest,
    mcp_tables: &[Vec<Object>],
) {
    let handler = mcp_tables[req.endpoint_index][req.method_index];

    // Convert json params to JSValue
    let Ok(params_val) = JSValue::from_serde_json(cx, &req.params) else {
        let _ = req.response_tx.try_send(McpChunkMessage {
            final_chunk: true,
            result_json: None,
            error: Some(McpChunkError {
                code: -32603,
                message: "failed to convert params to JS".into(),
            }),
        });
        return;
    };

    // Create MCP bridge carrying the mpsc sender (supports multiple chunks)
    let bridge_val = match create_mcp_bridge(cx, req.response_tx) {
        Ok(v) => v,
        Err(_) => {
            return;
        }
    };

    // Create a "chunk sender" function that the JS handler can call
    // to emit intermediate SSE events before returning the final result.
    // Usage in JS: await chunkSender({ results: [...], engine: "bing" })
    let chunk_fn = match cx.create_native_function_with_data(
        Callback::Func(mcp_send_chunk),
        "mcp_send_chunk",
        0,
        1,
        [bridge_val].into(),
    ) {
        Ok(f) => JSValue::from(f),
        Err(_) => {
            return;
        }
    };

    // Call JS handler with (params, chunkSender)
    match handler.call(cx, JSValue::UNDEFINED, &[params_val, chunk_fn]) {
        Ok(val) => {
            if let Ok(mut promise) = Promise::try_from(val) {
                let ok_fn = match cx.create_native_function_with_data(
                    Callback::Func(mcp_resolve_then),
                    "",
                    0,
                    0,
                    [bridge_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => {
                        return;
                    }
                };
                let fail_fn = match cx.create_native_function_with_data(
                    Callback::Func(mcp_reject_catch),
                    "",
                    1,
                    1,
                    [bridge_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => {
                        return;
                    }
                };
                let _ = promise.then(cx, ok_fn, fail_fn);
            } else {
                // Sync path: wrap raw result into MCP content format
                let to_json_result = val.to_serde_json(cx);
                let text = to_json_result
                    .ok()
                    .map(|v| match v {
                        serde_json::Value::String(s) => s,
                        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
                    })
                    .unwrap_or_default();
                let mcp_response = serde_json::json!({
                    "content": [{"type": "text", "text": text}]
                });
                let result_json = serde_json::to_string(&mcp_response).ok();
                let mut bv = bridge_val;
                let bridge = get_mcp_bridge_payload(cx, &mut bv).unwrap();
                bridge.send_final(McpChunkMessage {
                    final_chunk: true,
                    result_json,
                    error: None,
                });
            }
        }
        Err(_) => {
            let exc = cx.take_exception();
            let msg = exc.to_text(cx);
            let mcp_response = serde_json::json!({
                "content": [{"type": "text", "text": format!("Error: {}", msg.message)}],
                "isError": true
            });
            let result_json = serde_json::to_string(&mcp_response).ok();
            let mut bv = bridge_val;
            if let Ok(bridge) = get_mcp_bridge_payload(cx, &mut bv) {
                bridge.send_final(McpChunkMessage {
                    final_chunk: true,
                    result_json,
                    error: None,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MCP chunk sender (called by JS handler to emit intermediate SSE events)
// ---------------------------------------------------------------------------

/// JS callback: `chunkSender(data)` — sends an intermediate chunk via SSE.
/// `data` should be a JSON-serializable value (object, array, string, etc).
/// Returns `undefined` (fire-and-forget; if the channel is full the chunk is dropped).
fn mcp_send_chunk(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = cc.arg(0);
    let to_json_result = data.to_serde_json(ctx);
    let json = to_json_result
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_default();

    let _json_len = json.len();
    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_mcp_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_chunk(json);
    }

    Ok(JSValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// Promise resolve/reject callbacks
// ---------------------------------------------------------------------------

fn mcp_resolve_then(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let result = cc.arg(0);
    let to_json_result = result.to_serde_json(ctx);
    let text = to_json_result
        .ok()
        .map(|v| match v {
            serde_json::Value::String(s) => s,
            other => serde_json::to_string_pretty(&other).unwrap_or_default(),
        })
        .unwrap_or_default();

    let mcp_response = serde_json::json!({
        "content": [{"type": "text", "text": text}]
    });
    let result_json = serde_json::to_string(&mcp_response).ok();

    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_mcp_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_final(McpChunkMessage {
            final_chunk: true,
            result_json,
            error: None,
        });
    }

    Ok(JSValue::UNDEFINED)
}

fn mcp_reject_catch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    let message = if !reason.is_undefined() {
        let to_string_result = reason.to_string(ctx);
        to_string_result
            .ok()
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_else(|| "unknown error".into())
    } else {
        "unknown error".into()
    };

    let mcp_response = serde_json::json!({
        "content": [{"type": "text", "text": format!("Error: {}", message)}],
        "isError": true
    });
    let result_json = serde_json::to_string(&mcp_response).ok();

    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_mcp_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_final(McpChunkMessage {
            final_chunk: true,
            result_json,
            error: None,
        });
    }

    Ok(JSValue::UNDEFINED)
}
