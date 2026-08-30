// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Native MCP clients: `StdioMcpClient` / `HttpMcpClient`.
//!
//! Thin JS bindings over rmcp's client service: `connect()` runs the
//! initialize handshake, `listTools()` / `callTool()` are JSON-RPC requests,
//! `close()` gracefully tears down the transport (stdio: EOF → SIGKILL
//! ladder built into `TokioChildProcess`; http: session DELETE).
//!
//! The `RunningService` lives in a Send+Sync static registry keyed by an id
//! stored on the payload (native object payloads are !Send, tokio tasks can
//! not touch them directly).

use std::{
    collections::HashMap,
    sync::{
        LazyLock, Mutex as StdMutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use linkme::distributed_slice;
use process_wrap::tokio::CommandWrap;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::{
    model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation},
    service::{RoleClient, RunningService, ServiceExt},
    transport::{
        TokioChildProcess,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, PollSender, Runtime,
    ScopedContext, builtins::promise::Promise, def_native_object,
};

use crate::USER_CLASS_DEFS;

type Running = RunningService<RoleClient, ClientInfo>;

/// Per-request timeout default — mirrors ravenclaw's previous 30s bound.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

struct ClientState {
    /// Connected service; None until connect() completes.
    service: tokio::sync::Mutex<Option<Running>>,
    /// ServerPeerInfo captured after the handshake (for the serverInfo getter).
    server_info: StdMutex<Option<serde_json::Value>>,
    /// stdio only: spawned server pid, for exit-kill + post-close sweep.
    child_pid: StdMutex<Option<i32>>,
}

static CLIENTS: LazyLock<StdMutex<HashMap<i64, std::sync::Arc<ClientState>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));
static NEXT_CLIENT_ID: AtomicI64 = AtomicI64::new(1);

/// Pids of live stdio MCP server children. Killed (whole process, SIGKILL)
/// when aike exits — covers scripts that forget close() or die unexpectedly,
/// where rmcp's Drop-based cleanup may not run.
static LIVE_SERVER_PIDS: LazyLock<StdMutex<Vec<i32>>> = LazyLock::new(|| StdMutex::new(Vec::new()));

/// Kill every registered stdio MCP server child. Called from the engine's
/// atexit / signal-handler cleanup alongside `spawn::kill_all_children`.
pub(crate) fn kill_all_mcp_children() {
    let pids: Vec<i32> = match LIVE_SERVER_PIDS.try_lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
    for pid in pids {
        force_kill_child(pid);
    }
    LIVE_SERVER_PIDS.lock().unwrap().clear();
}

fn force_kill_child(pid: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    unsafe {
        crate::aike::terminate_process(pid);
    }
}

fn child_alive(pid: i32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, 0) == 0
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, WaitForSingleObject,
        };
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        alive
    }
}

async fn wait_child_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while child_alive(pid) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// Belt-and-suspenders sweep after rmcp teardown: SIGTERM, then SIGKILL.
/// (Windows collapses both stages to TerminateProcess.)
async fn sweep_child(pid: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    #[cfg(windows)]
    unsafe {
        crate::aike::terminate_process(pid);
    }

    if !wait_child_gone(pid, Duration::from_secs(2)).await {
        force_kill_child(pid);
        let _ = wait_child_gone(pid, Duration::from_secs(1)).await;
    }
}

/// Transport config captured at construction (plain data, moved into tasks).
pub(crate) enum ClientCfg {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Option<HashMap<String, String>>,
        cwd: Option<String>,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
    },
}

pub(crate) struct McpClientPayload {
    id: i64,
    timeout_ms: u64,
    cfg: ClientCfg,
}

impl McpClientPayload {
    /// Registry state; None once the client has been closed (entry removed).
    fn try_state(&self) -> Option<std::sync::Arc<ClientState>> {
        CLIENTS.lock().unwrap().get(&self.id).cloned()
    }

    /// State or a thrown "client is closed" error.
    fn state(&self, ctx: &mut ScopedContext) -> Result<std::sync::Arc<ClientState>, i32> {
        self.try_state()
            .ok_or_else(|| ctx.throw_internal_error("client is closed"))
    }
}

fn get_payload<'a>(
    ctx: &mut ScopedContext,
    this: &'a JSValue,
) -> Result<&'a McpClientPayload, i32> {
    JSStdioMcpClient::payload_of(this)
        .or_else(|| JSHttpMcpClient::payload_of(this))
        .ok_or_else(|| ctx.throw_type_error("not an Mcp client instance"))
}

/// Parse a string prop strictly: TypeError when present but not a string.
fn str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<String>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    match v.as_string(true) {
        Some(s) => Ok(Some(s.content().encode_utf8(false).to_string())),
        None => Err(ctx.throw_type_error(&format!("`{key}` must be a string"))),
    }
}

/// Parse an optional positive-integer millisecond option.
fn pos_ms_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<u64>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`{key}` must be a number")));
    }
    let n = v.to_number(ctx)?;
    let ok = matches!(n, zuqe::IntOrFloat::Int64(i) if i > 0)
        || matches!(n, zuqe::IntOrFloat::Float64(d) if d > 0.0 && d.fract() == 0.0);
    if !ok {
        return Err(ctx.throw_range_error(&format!("`{key}` must be a positive integer")));
    }
    Ok(Some(match n {
        zuqe::IntOrFloat::Int64(i) => i as u64,
        zuqe::IntOrFloat::Float64(d) => d as u64,
    }))
}

/// Parse `Record<string, string>` strictly; None when absent/undefined.
fn str_map_prop(
    ctx: &mut ScopedContext,
    obj: &Object,
    key: &str,
) -> Result<Option<HashMap<String, String>>, i32> {
    use zuqe::{JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK};
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let Some(mut map_obj) = v.as_object() else {
        return Err(ctx.throw_type_error(&format!("`{key}` must be an object")));
    };
    let pks = map_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    let mut out = HashMap::new();
    for pk in pks.iter() {
        let k = pk
            .atom
            .to_value(ctx)?
            .to_string(ctx)?
            .content()
            .encode_utf8(false)
            .to_string();
        let val = map_obj.get_property(ctx, pk.atom)?;
        let Some(s) = val.as_string(true) else {
            return Err(ctx.throw_type_error(&format!("`{key}` values must be strings")));
        };
        out.insert(k, s.content().encode_utf8(false).to_string());
    }
    Ok(Some(out))
}

/// Parse `args?: string[]` strictly.
fn args_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Vec<String>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(Vec::new());
    }
    let Some(arr) = v.as_object_Array() else {
        return Err(ctx.throw_type_error(&format!("`{key}` must be an array of strings")));
    };
    let mut out = Vec::with_capacity(arr.payload().values().len());
    for s in arr.payload().values() {
        let Some(s) = s.as_string(true) else {
            return Err(ctx.throw_type_error(&format!("`{key}` must be an array of strings")));
        };
        out.push(s.content().encode_utf8(false).to_string());
    }
    Ok(out)
}

/// Shared constructor body: validate config, register client state.
fn construct_client(ctx: &mut ScopedContext, cc: CallContext, kind: &str) -> JSResult {
    let Some(cfg) = cc.arg(0).as_object() else {
        return Err(ctx.throw_type_error(&format!("{kind}(config) — config must be an object")));
    };

    let timeout_ms = pos_ms_prop(ctx, &cfg, "timeoutMs")?.unwrap_or(DEFAULT_TIMEOUT_MS);

    let client_cfg = match kind {
        "StdioMcpClient" => {
            let command = str_prop(ctx, &cfg, "command")?
                .ok_or_else(|| ctx.throw_type_error("`command` is required"))?;
            if command.is_empty() {
                return Err(ctx.throw_type_error("`command` must not be empty"));
            }
            ClientCfg::Stdio {
                command,
                args: args_prop(ctx, &cfg, "args")?,
                env: str_map_prop(ctx, &cfg, "env")?,
                cwd: str_prop(ctx, &cfg, "cwd")?,
            }
        }
        _ => {
            let url = str_prop(ctx, &cfg, "url")?
                .ok_or_else(|| ctx.throw_type_error("`url` is required"))?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(ctx.throw_range_error("`url` must be an http:// or https:// URL"));
            }
            ClientCfg::Http {
                url,
                headers: str_map_prop(ctx, &cfg, "headers")?,
            }
        }
    };

    let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    CLIENTS.lock().unwrap().insert(
        id,
        std::sync::Arc::new(ClientState {
            service: tokio::sync::Mutex::new(None),
            server_info: StdMutex::new(None),
            child_pid: StdMutex::new(None),
        }),
    );

    JSStdioMcpClient::new_instance(
        ctx,
        JSValue::UNDEFINED,
        McpClientPayload {
            id,
            timeout_ms,
            cfg: client_cfg,
        },
    )
    .map(JSValue::from)
}

fn js_stdio_ctor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    construct_client(ctx, cc, "StdioMcpClient")
}
fn js_http_ctor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    construct_client(ctx, cc, "HttpMcpClient")
}

/// Build the rmcp transport + client info from the captured config.
fn make_client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("aike-mcp-client", "1.0.0"),
    )
}

/// Build the stdio child-process transport (stderr inherited = forwarded).
/// ProcessGroup gives us killpg (whole tree on unix); JobObject gives kernel
/// kill-on-close on Windows — so grandchildren are covered and the server
/// child cannot become an orphan.
fn make_stdio_transport(cfg: &ClientCfg) -> Result<TokioChildProcess, String> {
    let ClientCfg::Stdio {
        command,
        args,
        env,
        cwd,
    } = cfg
    else {
        return Err("not a stdio client".to_string());
    };

    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    if let Some(env) = env {
        cmd.env_clear();
        cmd.envs(env);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let mut wrap = CommandWrap::from(cmd);
    #[cfg(unix)]
    wrap.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrap.wrap(JobObject);

    TokioChildProcess::new(wrap).map_err(|e| format!("spawn failed: {e}"))
}

/// Build the streamable-http transport; custom headers ride on the reqwest
/// client's default header map.
fn make_http_transport(
    cfg: &ClientCfg,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, String> {
    let ClientCfg::Http { url, headers } = cfg else {
        return Err("not an http client".to_string());
    };

    let mut builder = reqwest::Client::builder();

    if let Some(headers) = headers {
        let mut hm = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name: reqwest::header::HeaderName =
                k.parse().map_err(|_| format!("invalid header name: {k}"))?;
            let value: reqwest::header::HeaderValue = v
                .parse()
                .map_err(|_| format!("invalid header value: {v}"))?;
            hm.insert(name, value);
        }
        builder = builder.default_headers(hm);
    }

    let http = builder.build().map_err(|e| format!("http client: {e}"))?;

    Ok(StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(url.clone()),
    ))
}

// The two transports have different concrete types; serve them via separate
// branches rather than a boxed trait to keep generics simple.

/// connect() — run the initialize handshake; resolves once connected.
fn js_connect(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let payload_state = {
        let this = cc.this();
        let p = get_payload(ctx, &this)?;
        (p.state(ctx)?, p.timeout_ms, extract_cfg(p))
    };
    let (state, timeout_ms, cfg) = payload_state;

    // Reject double-connect early (the task below would also overwrite).
    if state
        .service
        .try_lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
    {
        return Err(ctx.throw_type_error("client is already connected"));
    }

    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
            match payload {
                Ok(()) => Ok(JSValue::UNDEFINED),
                Err(e) => Err(cx.throw_internal_error(&e)),
            }
        })?;

    let wake = ctx.wake_handle();
    tokio::spawn(async move {
        let result = run_connect(state, timeout_ms, &cfg).await;
        if PollSender::from(tx).send_async(result).await.is_ok() {
            wake.wake();
        }
    });

    Ok(promise.into())
}

/// Clone the owned config out of the payload.
fn extract_cfg(p: &McpClientPayload) -> ClientCfg {
    match &p.cfg {
        ClientCfg::Stdio {
            command,
            args,
            env,
            cwd,
        } => ClientCfg::Stdio {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            cwd: cwd.clone(),
        },
        ClientCfg::Http { url, headers } => ClientCfg::Http {
            url: url.clone(),
            headers: headers.clone(),
        },
    }
}

async fn run_connect(
    state: std::sync::Arc<ClientState>,
    timeout_ms: u64,
    cfg: &ClientCfg,
) -> Result<(), String> {
    // The two transports have different concrete types; serve each in its own
    // branch rather than boxing to keep generics simple.
    let fut = async {
        match cfg {
            ClientCfg::Stdio { .. } => {
                let transport = make_stdio_transport(cfg)?;
                let child_pid = transport.id().map(|id| id as i32);
                let svc = make_client_info()
                    .serve(transport)
                    .await
                    .map_err(|e| format!("initialize failed: {e}"))?;
                *state.child_pid.lock().unwrap() = child_pid;
                if let Some(pid) = child_pid {
                    LIVE_SERVER_PIDS.lock().unwrap().push(pid);
                }
                Ok(svc)
            }
            ClientCfg::Http { .. } => {
                let transport = make_http_transport(cfg)?;
                make_client_info()
                    .serve(transport)
                    .await
                    .map_err(|e| format!("initialize failed: {e}"))
            }
        }
    };

    let svc = match tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await {
        Err(_elapsed) => Err(format!("connect timed out after {timeout_ms}ms")),
        Ok(r) => r,
    }?;

    // Capture server identity for the getter before publishing the service.
    let peer_info = svc.peer_info();
    *state.server_info.lock().unwrap() = peer_info
        .as_ref()
        .map(|pi| serde_json::to_value(pi).unwrap_or(serde_json::Value::Null));

    *state.service.lock().await = Some(svc);
    Ok(())
}

/// Run one request against the connected peer with the client's timeout.
async fn with_peer<T>(
    state: &std::sync::Arc<ClientState>,
    timeout_ms: u64,
    f: impl FnOnce(
        &rmcp::service::Peer<RoleClient>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, rmcp::service::ServiceError>> + Send + '_>,
    >,
) -> Result<T, String> {
    let guard = state.service.lock().await;
    let Some(svc) = guard.as_ref() else {
        return Err("client is not connected".to_string());
    };
    let fut = f(svc.peer());
    match tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await {
        Err(_elapsed) => Err(format!("request timed out after {timeout_ms}ms")),
        Ok(Err(e)) => Err(format!("{e}")),
        Ok(Ok(v)) => Ok(v),
    }
}

/// listTools() → `{ tools: [...], nextCursor? }` (raw ListToolsResult JSON).
fn js_list_tools(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let (state, timeout_ms) = {
        let this = cc.this();
        let p = get_payload(ctx, &this)?;
        (p.state(ctx)?, p.timeout_ms)
    };

    let (promise, tx) = Promise::create_with_channel::<Result<serde_json::Value, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
        },
    )?;

    let wake = ctx.wake_handle();
    tokio::spawn(async move {
        let result = with_peer(&state, timeout_ms, |peer| Box::pin(peer.list_tools(None)))
            .await
            .and_then(|r| serde_json::to_value(&r).map_err(|e| e.to_string()));
        if PollSender::from(tx).send_async(result).await.is_ok() {
            wake.wake();
        }
    });

    Ok(promise.into())
}

/// callTool(name, arguments?) → raw CallToolResult JSON
/// (`{ content: [...], structuredContent?, isError? }`).
fn js_call_tool(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let (state, timeout_ms) = {
        let this = cc.this();
        let p = get_payload(ctx, &this)?;
        (p.state(ctx)?, p.timeout_ms)
    };

    // Strict args validation on the JS thread.
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    if name.is_empty() {
        return Err(ctx.throw_type_error("`name` must not be empty"));
    }
    let args_val = cc.arg(1);
    let arguments: Option<serde_json::Map<String, serde_json::Value>> = if args_val.is_undefined() {
        None
    } else {
        let v = args_val
            .to_serde_json(ctx)
            .map_err(|_| ctx.throw_type_error("`arguments` must be an object"))?;
        match v {
            serde_json::Value::Object(m) => Some(m),
            _ => return Err(ctx.throw_type_error("`arguments` must be an object")),
        }
    };

    let (promise, tx) = Promise::create_with_channel::<Result<serde_json::Value, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
        },
    )?;

    let wake = ctx.wake_handle();
    tokio::spawn(async move {
        let result = with_peer(&state, timeout_ms, move |peer| {
            let mut params = CallToolRequestParams::new(name);
            params.arguments = arguments;
            Box::pin(async move {
                match peer.call_tool_once(params).await {
                    Ok(rmcp::model::CallToolResponse::Complete(result)) => Ok(result),
                    Ok(_) => Err(rmcp::service::ServiceError::UnexpectedResponse),
                    Err(e) => Err(e),
                }
            })
        })
        .await
        .and_then(|r| serde_json::to_value(&r).map_err(|e| e.to_string()));
        if PollSender::from(tx).send_async(result).await.is_ok() {
            wake.wake();
        }
    });

    Ok(promise.into())
}

/// close() — graceful teardown; resolves when the transport is down.
fn js_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let (state, id) = {
        let this = cc.this();
        let p = get_payload(ctx, &this)?;
        // Registry entry gone → close() already ran; resolve immediately.
        match p.try_state() {
            Some(state) => (state, p.id),
            None => return Ok(JSValue::UNDEFINED),
        }
    };

    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
            match payload {
                Ok(()) => Ok(JSValue::UNDEFINED),
                Err(e) => Err(cx.throw_internal_error(&e)),
            }
        })?;

    let wake = ctx.wake_handle();
    tokio::spawn(async move {
        // Take the service out first so concurrent calls see "not connected".
        let svc = state.service.lock().await.take();
        let result = match svc {
            None => Ok(()),
            Some(svc) => {
                // Bounded teardown: transport close waits for the child to
                // exit (≤3s inside TokioChildProcess); don't hang past that.
                match tokio::time::timeout(Duration::from_secs(6), svc.cancel()).await {
                    Err(_elapsed) => Err("close timed out".to_string()),
                    Ok(_) => Ok(()),
                }
            }
        };
        // Sweep the stdio child if it somehow survived rmcp teardown:
        // SIGTERM → 2s → SIGKILL → 1s. Then drop the exit-kill registration.
        let pid = state.child_pid.lock().unwrap().take();
        if let Some(pid) = pid {
            sweep_child(pid).await;
            LIVE_SERVER_PIDS.lock().unwrap().retain(|&p| p != pid);
        }
        CLIENTS.lock().unwrap().remove(&id);
        if PollSender::from(tx).send_async(result).await.is_ok() {
            wake.wake();
        }
    });

    Ok(promise.into())
}

/// serverInfo — handshake result after connect(), null before.
fn js_server_info(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    let Some(state) = p.try_state() else {
        return Ok(JSValue::NULL);
    };
    let guard = state.server_info.lock().unwrap();
    match guard.as_ref() {
        Some(v) => JSValue::from_serde_json(ctx, v),
        None => Ok(JSValue::NULL),
    }
}

const CLIENT_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("connect", 0, js_connect, 0),
    FieldDef::FUNC("listTools", 0, js_list_tools, 0),
    FieldDef::FUNC("callTool", 2, js_call_tool, 0),
    FieldDef::FUNC("close", 0, js_close, 0),
    FieldDef::GETSET("serverInfo", Some(js_server_info), None, 0),
];

const STDIO_CLIENT_CTOR: ClassCtorDef = ClassCtorDef::new(js_stdio_ctor, 1, true);
const HTTP_CLIENT_CTOR: ClassCtorDef = ClassCtorDef::new(js_http_ctor, 1, true);

def_native_object!(
    "StdioMcpClient",
    JSStdioMcpClient,
    McpClientPayload,
    CLIENT_PROTO,
    Some(STDIO_CLIENT_CTOR)
);

def_native_object!(
    "HttpMcpClient",
    JSHttpMcpClient,
    McpClientPayload,
    CLIENT_PROTO,
    Some(HTTP_CLIENT_CTOR)
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_stdio_mcp_client(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSStdioMcpClient::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_mcp_client(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpMcpClient::register(rt)
}
