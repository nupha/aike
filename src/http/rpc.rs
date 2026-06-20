// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

// JSON-RPC 2.0 and MCP (Model Context Protocol) support for HttpServer.

use std::{
    borrow::Cow,
    cell::Cell,
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
        ProtocolVersion, ServerCapabilities, ServerInfo, ServerNotification,
    },
    service::{RequestContext, RoleServer},
};
use tokio::sync::oneshot;
use zuqe::{
    CallContext, Callback, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::class_http_server::IncomingRequest;

/// A JSON-RPC mount point with method handlers.
pub(super) struct JsRpcEndpoint {
    pub(super) mount_path: String,
    /// (method_name, js_handler)
    pub(super) methods: Vec<(String, Object)>,
}

impl JsRpcEndpoint {
    pub(super) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for (_, handler) in &self.methods {
            handler.trace(gcx);
        }
    }
}

/// JSON-RPC method request sent via crossbeam channel.
pub(super) struct RpcRequest {
    pub(super) endpoint_index: usize,
    pub(super) method_index: usize,
    pub(super) params_json: String,
    pub(super) response_tx: oneshot::Sender<RpcResponseMessage>,
}

/// JSON-RPC response sent back via oneshot channel.
pub(super) struct RpcResponseMessage {
    pub(super) result_json: Option<String>,
    pub(super) error: Option<RpcError>,
}

pub(super) struct RpcError {
    pub(super) code: i64,
    pub(super) message: String,
}

// ---------------------------------------------------------------------------
// MCP (rmcp-based)
// ---------------------------------------------------------------------------

/// Native tool handler function type.
pub(super) type NativeMcpToolFn = Arc<
    dyn Fn(
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>
        + Send
        + Sync,
>;

/// MCP schemas shared between tokio closures (Send + Sync).
/// Does NOT contain Object (which is !Send).
pub(super) struct McpSchemas {
    /// All tool schemas (native + JS), as rmcp::model::Tool values
    pub(super) tools: Vec<rmcp::model::Tool>,
    /// Tool name -> index in tools
    pub(super) name_to_index: HashMap<String, usize>,
    /// Tool name -> index in native_handlers (only for native tools)
    pub(super) native_map: HashMap<String, usize>,
}

/// MCP endpoint state — holds everything needed to construct an rmcp service.
pub(super) struct McpEndpoint {
    pub(super) mount_path: String,
    pub(super) server_name: String,
    pub(super) server_version: String,
    /// If true, use stateful SSE mode (session-based, text/event-stream).
    /// If false, use stateless JSON mode (no session, application/json).
    pub(super) stateful: bool,
    /// Shared schemas accessible from tokio (Send + Sync)
    pub(super) shared: Arc<RwLock<McpSchemas>>,
    /// Native handlers: (name, schema, handler_fn)
    pub(super) handlers: Vec<(String, serde_json::Value, NativeMcpToolFn)>,
    /// JS handlers (JS thread only, not Send)
    pub(super) js_handlers: Vec<Object>,
    /// Channel to send handler table updates to the poller
    pub(super) update_tx: Option<channel::Sender<Vec<Object>>>,
}

impl McpEndpoint {
    pub(super) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for handler in &self.js_handlers {
            handler.trace(gcx);
        }
    }
}

/// MCP tool call request sent via crossbeam channel to the JS thread.
/// Uses mpsc so the JS handler can send multiple chunks (SSE events) before
/// the final result. The final message has `final_chunk: true`.
pub(super) struct McpToolCallRequest {
    pub(super) endpoint_index: usize,
    pub(super) method_index: usize,
    pub(super) params: serde_json::Value,
    pub(super) response_tx: tokio::sync::mpsc::Sender<McpChunkMessage>,
}

/// A single chunk sent from JS tool handler to Rust MCP service.
/// Either an intermediate SSE notification event or the final CallToolResult.
pub(super) struct McpChunkMessage {
    /// If true, this is the final message — the stream ends after this.
    pub(super) final_chunk: bool,
    /// For intermediate chunks: the JSON-RPC notification payload (as JSON string).
    /// For the final chunk: the full CallToolResult JSON (as before).
    pub(super) result_json: Option<String>,
    /// Error (only meaningful for the final chunk).
    pub(super) error: Option<RpcError>,
}

// ---------------------------------------------------------------------------
// Bridge: wraps oneshot::Sender<RpcResponseMessage> as a JS object (JSON-RPC)
// ---------------------------------------------------------------------------

pub(super) struct RpcBridgePayload {
    tx: Option<oneshot::Sender<RpcResponseMessage>>,
}

impl RpcBridgePayload {
    fn send_response(&mut self, msg: RpcResponseMessage) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(msg);
        }
    }
}

fn get_bridge_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut RpcBridgePayload, i32> {
    val.as_native_object_mut::<JSRpcBridge>()
        .and_then(|o| o.payload_mut())
        .ok_or_else(|| {
            ctx.throw_internal_error("RpcBridge was detached");
            -1
        })
}

#[allow(dead_code)]
fn js_bridge_ctor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error("RpcBridge cannot be constructed from JS");
    Err(-1)
}

const BRIDGE_PROTO: &[FieldDef] = &[];

def_native_object!(
    "RpcBridge",
    JSRpcBridge,
    RpcBridgePayload,
    BRIDGE_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_rpc_bridge(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSRpcBridge::register(rt)
}

/// Create a JSRpcBridge instance wrapping the given oneshot sender.
fn create_bridge(
    ctx: &mut ScopedContext,
    tx: oneshot::Sender<RpcResponseMessage>,
) -> Result<JSValue, i32> {
    JSRpcBridge::new_instance(ctx, JSValue::UNDEFINED, RpcBridgePayload { tx: Some(tx) })
        .map(|v| v.into())
}

// ---------------------------------------------------------------------------
// MCP Bridge: wraps mpsc::Sender<McpChunkMessage> for streaming tool results
// ---------------------------------------------------------------------------

pub(super) struct McpBridgePayload {
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

// ---------------------------------------------------------------------------
// JSON-RPC router builder (ajj)
// ---------------------------------------------------------------------------

/// Build an ajj::Router from RPC method definitions, routing calls through
/// the shared channel to the JS thread.
///
/// Returns (router, handler_table) where handler_table maps method_index
/// to JS handler objects for use in the poller.
pub(super) fn build_js_rpc_router(
    methods: &[(String, Object)],
    tx: channel::Sender<IncomingRequest>,
    endpoint_index: usize,
) -> (ajj::Router<()>, Vec<Object>) {
    let mut router = ajj::Router::<()>::new();
    let mut handler_table = Vec::new();

    for (i, (method_name, callback)) in methods.iter().enumerate() {
        handler_table.push(*callback);
        let tx = tx.clone();

        router = router.route(method_name.to_owned(), move |params: serde_json::Value| {
            let tx = tx.clone();
            let params_str = params.to_string();
            async move {
                let (resp_tx, resp_rx) = oneshot::channel();
                let _ = tx.send(IncomingRequest::Rpc(RpcRequest {
                    endpoint_index,
                    method_index: i,
                    params_json: params_str,
                    response_tx: resp_tx,
                }));
                match resp_rx.await {
                    Ok(resp) => {
                        if let Some(err) = resp.error {
                            Err(ajj::ErrorPayload {
                                code: err.code,
                                message: Cow::Owned(err.message),
                                data: None::<()>,
                            })
                        } else {
                            let val: serde_json::Value = serde_json::from_str(
                                &resp.result_json.unwrap_or_else(|| "null".into()),
                            )
                            .unwrap_or(serde_json::Value::Null);
                            Ok(val)
                        }
                    }
                    _ => Err(ajj::ErrorPayload::internal_error_message(
                        "handler channel closed".into(),
                    )),
                }
            }
        });
    }

    (router, handler_table)
}

// ---------------------------------------------------------------------------
// MCP service (rmcp-based) — dynamic tool dispatch
// ---------------------------------------------------------------------------

/// The MCP service that bridges rmcp protocol to our native + JS tool handlers.
///
/// Implements `ServerHandler` manually (not via `#[tool_router]`) because tools
/// are dynamic — JS tools are defined at runtime via the JS API.
#[derive(Clone)]
pub(super) struct McpService {
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
}

impl McpService {
    pub(super) fn new(
        shared: Arc<RwLock<McpSchemas>>,
        handlers: Vec<(String, serde_json::Value, NativeMcpToolFn)>,
        js_tx: channel::Sender<IncomingRequest>,
        endpoint_index: usize,
        server_name: String,
        server_version: String,
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
        }
    }

    /// Convert an McpEndpoint into an McpService.
    pub(super) fn from_endpoint(
        ep: &McpEndpoint,
        js_tx: channel::Sender<IncomingRequest>,
        endpoint_index: usize,
    ) -> Self {
        Self::new(
            ep.shared.clone(),
            ep.handlers.clone(),
            js_tx,
            endpoint_index,
            ep.server_name.clone(),
            ep.server_version.clone(),
        )
    }
}

impl ServerHandler for McpService {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        ServerInfo::new(capabilities)
            .with_server_info(Implementation::new(
                self.server_name.clone(),
                self.server_version.clone(),
            ))
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
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
            let send_result = self
                .js_tx
                .send(IncomingRequest::McpToolCall(McpToolCallRequest {
                    endpoint_index: self.endpoint_index,
                    method_index,
                    params: arguments,
                    response_tx: resp_tx,
                }));

            // Test: send a "start" notification before JS handler runs.
            // This verifies that SSE streaming notifications work end-to-end.
            {
                let start_notif = CustomNotification::new(
                    "notifications/tool-chunk",
                    Some(serde_json::json!({"status": "started", "tool": tool_name})),
                );
                let _ = context
                    .peer
                    .send_notification(ServerNotification::CustomNotification(start_notif))
                    .await;
                eprintln!("[MCP] Sent 'started' notification for tool: {}", tool_name);
            }

            // Read chunks from the JS handler.
            // Intermediate chunks → SSE notification events.
            // Final chunk → CallToolResult response.
            loop {
                match resp_rx.recv().await {
                    Some(chunk) => {
                        if chunk.final_chunk {
                            eprintln!("[MCP] received final chunk for tool: {}", tool_name);
                            if let Some(err) = chunk.error {
                                eprintln!(
                                    "[MCP] final chunk has error: code={} msg={}",
                                    err.code, err.message
                                );
                                return Ok(CallToolResult::error(vec![Content::text(format!(
                                    "Error: {}",
                                    err.message
                                ))]));
                            }
                            let result_json = chunk.result_json.as_deref().unwrap_or("null");
                            eprintln!(
                                "[MCP] final chunk result (first 300 chars): {}",
                                &result_json[..result_json.len().min(300)]
                            );
                            return match serde_json::from_str::<CallToolResult>(result_json) {
                                Ok(ctr) => Ok(ctr),
                                Err(e) => {
                                    eprintln!(
                                        "[MCP] failed to parse final chunk as CallToolResult: {:?}",
                                        e
                                    );
                                    Ok(CallToolResult::success(vec![Content::text(
                                        result_json.to_string(),
                                    )]))
                                }
                            };
                        } else {
                            // Intermediate chunk: send as custom notification (SSE event)
                            if let Some(ref json) = chunk.result_json {
                                eprintln!(
                                    "[MCP] sending intermediate chunk ({} bytes)",
                                    json.len()
                                );
                                let notif = CustomNotification::new(
                                    "notifications/tool-chunk",
                                    Some(
                                        serde_json::from_str::<serde_json::Value>(json)
                                            .unwrap_or(serde_json::Value::String(json.clone())),
                                    ),
                                );
                                let send_result = context
                                    .peer
                                    .send_notification(ServerNotification::CustomNotification(
                                        notif,
                                    ))
                                    .await;
                                eprintln!("[MCP] send_notification result: {:?}", send_result);
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
// MCP transport builders
// ---------------------------------------------------------------------------

/// Build an rmcp stdio server future for the given endpoint.
///
/// This spawns a tokio task that serves the MCP protocol over stdin/stdout.
#[allow(dead_code)]
pub(super) fn build_stdio_mcp(
    ep: &McpEndpoint,
    js_tx: channel::Sender<IncomingRequest>,
    endpoint_index: usize,
) -> tokio::task::JoinHandle<Result<(), rmcp::service::ServerInitializeError>> {
    let service = McpService::from_endpoint(ep, js_tx, endpoint_index);
    tokio::spawn(async move {
        let transport = (tokio::io::stdin(), tokio::io::stdout());
        rmcp::serve_server(service, transport)
            .await
            .map(|_running| ())
    })
}

/// Build an axum Router for the MCP service using Streamable HTTP.
///
/// The returned Router can be merged into an existing axum app via
/// `router.merge(mcp_router)`.
pub(super) fn build_axum_mcp_route(
    ep: &McpEndpoint,
    js_tx: channel::Sender<IncomingRequest>,
    endpoint_index: usize,
) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };

    let service = McpService::from_endpoint(ep, js_tx, endpoint_index);
    // Choose mode based on endpoint configuration.
    // - stateful SSE mode: session-based, text/event-stream responses, supports streaming chunks.
    // - stateless JSON mode: no session, plain application/json, lower overhead.
    let config = if ep.stateful {
        StreamableHttpServerConfig::default()
            .with_stateful_mode(true)
            .disable_allowed_hosts()
    } else {
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts()
    };
    let session_manager = Arc::new(LocalSessionManager::default());

    let mcp_svc = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(service.clone()),
        session_manager,
        config,
    );

    axum::Router::new().route(&ep.mount_path, axum::routing::any_service(mcp_svc))
}

// ---------------------------------------------------------------------------
// Poller-side handlers (run on JS thread)
// ---------------------------------------------------------------------------

/// Handle a JSON-RPC request on the JS thread by calling the JS handler.
pub(super) fn handle_rpc_request(cx: &mut ScopedContext, req: RpcRequest, tables: &[Vec<Object>]) {
    let handler = &tables[req.endpoint_index][req.method_index];

    // Parse params JSON and convert to JS value
    let params_val = match serde_json::from_str::<serde_json::Value>(&req.params_json) {
        Ok(v) => match JSValue::from_serde_json(cx, &v) {
            Ok(val) => val,
            Err(_) => {
                let _ = req.response_tx.send(RpcResponseMessage {
                    result_json: None,
                    error: Some(RpcError {
                        code: -32603,
                        message: "failed to convert params to JS".into(),
                    }),
                });
                return;
            }
        },
        Err(_) => {
            // Params is null/undefined for omitted params
            JSValue::UNDEFINED
        }
    };

    // Create bridge object carrying the oneshot sender
    let bridge_val = match create_bridge(cx, req.response_tx) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Call JS handler (must be async function returning Promise)
    match handler.call(cx, JSValue::UNDEFINED, &[params_val]) {
        Ok(val) => {
            if let Ok(mut promise) = Promise::try_from(val) {
                // Attach then/catch with bridge as data
                let ok_fn = match cx.create_native_function_with_data(
                    Callback::Func(rpc_resolve_then),
                    "",
                    0,
                    0,
                    [bridge_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => return,
                };
                let fail_fn = match cx.create_native_function_with_data(
                    Callback::Func(rpc_reject_catch),
                    "",
                    1,
                    1,
                    [bridge_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => return,
                };
                let _ = promise.then(cx, ok_fn, fail_fn);
            } else {
                // Non-Promise result (defensive: handler should be async)
                let result_json = val
                    .to_serde_json(cx)
                    .ok()
                    .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "null".into()));
                let mut bv = bridge_val;
                let bridge = get_bridge_payload(cx, &mut bv).unwrap();
                bridge.send_response(RpcResponseMessage {
                    result_json,
                    error: None,
                });
            }
        }
        Err(_) => {
            let exc = cx.take_exception();
            let msg = exc.to_text(cx);
            let mut bv = bridge_val;
            if let Ok(bridge) = get_bridge_payload(cx, &mut bv) {
                bridge.send_response(RpcResponseMessage {
                    result_json: None,
                    error: Some(RpcError {
                        code: -32000,
                        message: msg.message,
                    }),
                });
            }
        }
    }
}

/// Handle an MCP tool call request on the JS thread.
/// Looks up the handler from the JS-thread handler table by method_index.
/// Supports streaming: the JS handler can send intermediate chunks via the
/// bridge before sending the final result.
pub(super) fn handle_mcp_tool_call(
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
            error: Some(RpcError {
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

    let json_len = json.len();
    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_mcp_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_chunk(json);
    }

    Ok(JSValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// Promise resolve/reject callbacks
// ---------------------------------------------------------------------------

fn rpc_resolve_then(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let result = cc.arg(0);

    let result_json = result
        .to_serde_json(ctx)
        .ok()
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "null".into()));

    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_response(RpcResponseMessage {
            result_json,
            error: None,
        });
    }

    Ok(JSValue::UNDEFINED)
}

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

fn rpc_reject_catch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    let message = if !reason.is_undefined() {
        reason
            .to_string(ctx)
            .ok()
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_else(|| "unknown error".into())
    } else {
        "unknown error".into()
    };

    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_response(RpcResponseMessage {
            result_json: None,
            error: Some(RpcError {
                code: -32000,
                message,
            }),
        });
    }

    Ok(JSValue::UNDEFINED)
}
