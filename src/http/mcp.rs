// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
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
use zuqe::{
    CallContext, Callback, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::class_http_server::IncomingRequest;

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
    pub(crate) tools: Vec<rmcp::model::Tool>,
    /// Tool name -> index in tools
    pub(crate) name_to_index: HashMap<String, usize>,
    /// Tool name -> index in native_handlers (only for native tools)
    pub(crate) native_map: HashMap<String, usize>,
}

/// MCP endpoint state — holds everything needed to construct an rmcp service.
pub(crate) struct McpEndpoint {
    pub(crate) mount_path: String,
    pub(crate) server_name: String,
    pub(crate) server_version: String,
    /// Shared schemas accessible from tokio (Send + Sync)
    pub(crate) shared: Arc<RwLock<McpSchemas>>,
    /// Native handlers: (name, schema, handler_fn)
    pub(crate) handlers: Vec<(String, serde_json::Value, NativeMcpToolFn)>,
    /// JS handlers (JS thread only, not Send)
    pub(crate) js_handlers: Vec<Object>,
    /// Channel to send handler table updates to the poller
    pub(crate) update_tx: Option<channel::Sender<Vec<Object>>>,
}

impl McpEndpoint {
    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for handler in &self.js_handlers {
            handler.trace(gcx);
        }
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

// ---------------------------------------------------------------------------
// MCP service (rmcp-based) — dynamic tool dispatch
// ---------------------------------------------------------------------------

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
}

impl McpService {
    pub(crate) fn new(
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
    pub(crate) fn from_endpoint(
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
// MCP transport builders
// ---------------------------------------------------------------------------

/// Build an rmcp stdio server future for the given endpoint.
///
/// This spawns a tokio task that serves the MCP protocol over stdin/stdout.
#[allow(dead_code)]
pub(crate) fn build_stdio_mcp(
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
pub(crate) fn build_axum_mcp_route(
    ep: &McpEndpoint,
    js_tx: channel::Sender<IncomingRequest>,
    endpoint_index: usize,
) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };

    let service = McpService::from_endpoint(ep, js_tx, endpoint_index);
    // Always use stateful SSE mode (session-based, text/event-stream).
    // Stateless JSON mode breaks SSE notifications and streaming chunks.
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(true)
        .disable_allowed_hosts();
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
