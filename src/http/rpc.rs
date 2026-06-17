// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

// JSON-RPC 2.0 support for HttpServer.
//
// Provides data structures and functions for routing JSON-RPC requests
// through ajj::Router to JS callbacks via the unified crossbeam channel.

use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

use crossbeam::channel;
use linkme::distributed_slice;
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

// --- MCP (Model Context Protocol) support ---

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
    /// All tool schemas (native + JS), handler field stripped
    pub(super) schemas: Vec<serde_json::Value>,
    /// Tool name -> index in schemas
    pub(super) name_to_index: HashMap<String, usize>,
    /// Tool name -> index in native_handlers (only for native tools)
    pub(super) native_map: HashMap<String, usize>,
}

/// MCP endpoint state.
pub(super) struct McpEndpoint {
    pub(super) mount_path: String,
    pub(super) server_name: String,
    pub(super) server_version: String,
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

/// MCP tool call request sent via crossbeam channel.
/// Uses method_index (not Object) since Object is not Send.
pub(super) struct McpToolCallRequest {
    pub(super) endpoint_index: usize,
    pub(super) method_index: usize,
    pub(super) params: serde_json::Value,
    pub(super) response_tx: oneshot::Sender<RpcResponseMessage>,
}

// --- Bridge: wraps oneshot::Sender<RpcResponseMessage> as a JS object ---

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

/// Build an ajj::Router for MCP protocol methods.
///
/// Registers 4 protocol routes, all handled directly in Rust.
/// `initialize` and `notifications/initialized` return immediately.
/// `tools/list` reads from shared schemas (Send+Sync).
/// `tools/call` dispatches to native handlers directly or sends JS handler
/// calls through the crossbeam channel using method_index.
///
/// Returns (router, initial_js_handlers).
pub(super) fn build_mcp_router(
    ep: &McpEndpoint,
    tx: channel::Sender<IncomingRequest>,
    endpoint_index: usize,
) -> (ajj::Router<()>, Vec<Object>) {
    let shared = ep.shared.clone();
    let shared2 = shared.clone();
    let native_fns: Arc<Vec<NativeMcpToolFn>> =
        Arc::new(ep.handlers.iter().map(|(_, _, f)| f.clone()).collect());

    let server_info = serde_json::json!({
        "name": ep.server_name,
        "version": ep.server_version,
    });

    let mut router = ajj::Router::<()>::new();

    // initialize — Rust direct response
    router = router.route(
        "initialize".to_owned(),
        move |_params: serde_json::Value| {
            let si = server_info.clone();
            async move {
                Ok::<_, ajj::ErrorPayload<()>>(serde_json::json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": si,
                }))
            }
        },
    );

    // notifications/initialized — Rust direct (returns null)
    router = router.route(
        "notifications/initialized".to_owned(),
        move |_params: serde_json::Value| async move {
            Ok::<_, ajj::ErrorPayload<()>>(serde_json::Value::Null)
        },
    );

    // tools/list — Rust reads shared schemas (Send + Sync)
    router = router.route(
        "tools/list".to_owned(),
        move |_params: serde_json::Value| {
            let s = shared.clone();
            async move {
                let schemas = s.read().unwrap().schemas.clone();
                Ok::<_, ajj::ErrorPayload<()>>(serde_json::json!({ "tools": schemas }))
            }
        },
    );

    // tools/call — dynamic dispatch: native tools run directly, JS tools go through channel
    router = router.route("tools/call".to_owned(), move |params: serde_json::Value| {
        let s = shared2.clone();
        let tx = tx.clone();
        let nf = native_fns.clone();

        async move {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            // Read shared schemas to resolve tool index
            let dispatch = {
                let guard = s.read().unwrap();
                guard
                    .name_to_index
                    .get(&name)
                    .map(|&idx| (idx, guard.native_map.get(&name).copied()))
            };

            let Some((method_index, native_idx)) = dispatch else {
                return Ok::<_, ajj::ErrorPayload<()>>(serde_json::json!({
                    "content": [{"type": "text", "text": format!("Error: Unknown tool: {}", name)}],
                    "isError": true
                }));
            };

            if let Some(ni) = native_idx {
                // Native handler — call directly (no channel)
                match nf[ni](args).await {
                    Ok(result) => Ok(result),
                    Err(msg) => Ok(serde_json::json!({
                        "content": [{"type": "text", "text": format!("Error: {}", msg)}],
                        "isError": true
                    })),
                }
            } else {
                // JS handler — send method_index through channel
                let (resp_tx, resp_rx) = oneshot::channel();
                let _ = tx.send(IncomingRequest::McpToolCall(McpToolCallRequest {
                    endpoint_index,
                    method_index,
                    params: args,
                    response_tx: resp_tx,
                }));
                match resp_rx.await {
                    Ok(resp) => convert_rpc_response_to_mcp(resp),
                    _ => Err(ajj::ErrorPayload::internal_error_message(
                        "handler channel closed".into(),
                    )),
                }
            }
        }
    });

    (router, ep.js_handlers.clone())
}

/// Convert RpcResponseMessage to MCP-format ajj result.
fn convert_rpc_response_to_mcp(
    resp: RpcResponseMessage,
) -> Result<serde_json::Value, ajj::ErrorPayload<()>> {
    if let Some(err) = resp.error {
        Err(ajj::ErrorPayload {
            code: err.code,
            message: Cow::Owned(err.message),
            data: None::<()>,
        })
    } else {
        let val: serde_json::Value =
            serde_json::from_str(&resp.result_json.unwrap_or_else(|| "null".into()))
                .unwrap_or(serde_json::Value::Null);
        Ok(val)
    }
}

// --- Poller-side handlers (run on JS thread) ---

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
pub(super) fn handle_mcp_tool_call(
    cx: &mut ScopedContext,
    req: McpToolCallRequest,
    mcp_tables: &[Vec<Object>],
) {
    let handler = mcp_tables[req.endpoint_index][req.method_index];

    // Convert json params to JSValue
    let params_val = match JSValue::from_serde_json(cx, &req.params) {
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
    };

    // Create bridge object carrying the oneshot sender
    let bridge_val = match create_bridge(cx, req.response_tx) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Call JS handler
    match handler.call(cx, JSValue::UNDEFINED, &[params_val]) {
        Ok(val) => {
            if let Ok(mut promise) = Promise::try_from(val) {
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

/// Promise resolve callback — serialize result and send via bridge.
fn rpc_resolve_then(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let result = cc.arg(0);

    // Serialize result to JSON
    let result_json = result
        .to_serde_json(ctx)
        .ok()
        .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "null".into()));

    // Extract bridge and send response
    let mut bridge_val = cc.data(0);
    if let Ok(bridge) = get_bridge_payload(ctx, &mut bridge_val) {
        bridge.send_response(RpcResponseMessage {
            result_json,
            error: None,
        });
    }

    Ok(JSValue::UNDEFINED)
}

/// Promise reject callback — extract error and send via bridge.
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
