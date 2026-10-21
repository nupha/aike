// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! `Aike.createRuntime` — multi-runtime per thread: create and drive child
//! runtimes (independent GC heap + context) from a parent script.
//!
//! Design: `multi-runtime-js-api.md`. The child runs on the same thread and is
//! cooperatively advanced by the parent's event loop: this module registers a
//! tick handler on the parent context and, on every parent tick, drives each
//! child with its own `run_async()` future using a "bridge" waker that forwards
//! the child's park wakes to the parent's park. `run()` is therefore async: the
//! parent only advances the child while it awaits.
//!
//! Cross-runtime values use the persistent bytecode codec (`JSValue::dump` /
//! `JSValue::restore`) — the same mechanism as `Aike.dumpValue`. Only
//! structured-cloneable values may cross; functions/promises/symbols throw.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    time::{Duration, Instant},
};

use zuqe::{
    Array, CallContext, Callback, ClassID, Context, EvalFlags, EvalName, EvalType, Exception,
    JS_CLASS_ARRAY, JS_CLASS_BOOLEAN, JS_CLASS_ERROR, JS_CLASS_MAP, JS_CLASS_NUMBER,
    JS_CLASS_OBJECT, JS_CLASS_PROMISE, JS_CLASS_SET, JS_CLASS_STRING, JS_CLASS_WEAKMAP,
    JS_CLASS_WEAKSET, JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK, JS_PROP_C_W_E, JS_PROP_ENUMERABLE,
    JSResult, JSValue, JobState, NativeObject, Object, Runtime, ScopedContext, WakeHandle,
    builtins::promise::{Promise, PromiseResultSlot, PromiseState},
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::whatwg::{
    abort, base64, blob, console, crypto, dom, fetch, form_data, microtask, stream,
    structured_clone, text_decoder, text_encoder, timeout, url,
};

/// Child global prefix for an in-flight run's completion promise (one key per
/// run so concurrent runs don't collide).
const RESULT_KEY_PREFIX: &str = "__aike_run_result_";

// Process argv/exec info, stashed by `setup_globals` so a child granted the
// `process` capability can build an equivalent namespace.
thread_local! {
    static PROCESS_INFO: RefCell<(Vec<String>, Vec<String>, String)> =
        const { RefCell::new((Vec::new(), Vec::new(), String::new())) };
}

pub(crate) fn save_process_info(argv: &[String], exec_argv: &[String], exec_path: &str) {
    PROCESS_INFO.with(|p| {
        *p.borrow_mut() = (argv.to_vec(), exec_argv.to_vec(), exec_path.to_string());
    });
}

// ---------------------------------------------------------------------------
// Run outcome crossing the boundary (serialized)
// ---------------------------------------------------------------------------

/// A single `run()` result, transferred to the parent promise via
/// [`PromiseResultSlot`]. Blob-based so no child/heap JSValue leaks.
enum RunOutcome {
    Value(Vec<u8>),
    Error {
        name: String,
        message: String,
        stack: Option<String>,
    },
}

/// Converter run in the parent context: resolve with the restored value, or
/// reject with a serializable error object `{name, message, stack}`.
fn make_run_promise(ctx: &mut ScopedContext, outcome: RunOutcome) -> JSResult {
    match outcome {
        RunOutcome::Value(blob) => JSValue::restore(ctx, &blob)
            .map_err(|e| ctx.throw_type_error(&format!("runtime.run: {e}"))),
        RunOutcome::Error {
            name,
            message,
            stack,
        } => {
            fn set_prop(
                ctx: &mut ScopedContext,
                o: &mut Object,
                k: &str,
                v: String,
            ) -> Result<(), i32> {
                let s = ctx
                    .create_string_from_utf8(v.into_bytes())
                    .map(JSValue::make_string)?;
                o.define_property_value_str(ctx, k, s, JS_PROP_ENUMERABLE)?;
                Ok(())
            }
            let mut o = ctx.create_base_object()?;
            set_prop(ctx, &mut o, "name", name)?;
            set_prop(ctx, &mut o, "message", message)?;
            if let Some(stack) = stack {
                set_prop(ctx, &mut o, "stack", stack)?;
            }
            ctx.throw(Exception::Error(o.into()));
            Err(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Shared handle to a run's result slot: the run job commits the outcome;
/// `close` can take any still-uncommitted slot to reject it.
type SharedSlot = Rc<RefCell<Option<PromiseResultSlot<RunOutcome>>>>;

struct Child {
    rt: Runtime,
    ctx: Context<'static>,
    /// The import policy of this runtime, as a **parent-heap** JSValue.
    ///
    /// The policy is the creator's function, so it lives in the creator's
    /// heap; it is traced by the root's GC trace handler (see
    /// `gc_trace_runtime_handles`). `None` means "no policy given" — which
    /// denies every import.
    import_policy: Option<JSValue>,
    /// Per-run wall-clock timeout (ms); `None`/0 = unlimited.
    default_timeout: Option<u64>,
    /// Parent-heap handle object — traced by `gc_trace_runtime_handles`.
    handle: JSValue,
    /// Read by the child's interrupt handler to abort overrunning sync code.
    deadline: Rc<Cell<Option<Instant>>>,

    /// Parent → child message blobs awaiting delivery.
    in_msg: RefCell<VecDeque<Vec<u8>>>,
    /// Child → parent message blobs awaiting delivery.
    out_msg: RefCell<VecDeque<Vec<u8>>>,

    /// In-flight run slots (taken and rejected on `close`).
    run_slots: RefCell<Vec<SharedSlot>>,
    closed: Cell<bool>,
}

thread_local! {
    static CHILDREN: RefCell<HashMap<u32, Rc<Child>>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    /// Root runtime handle: `createRuntime`/`close` register/unregister children
    /// on it (the engine tick drives the whole tree from the root).
    static ROOT_RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    /// Root runtime's park wake handle; child → parent messages wake the root
    /// so its event loop re-polls and delivers them.
    static ROOT_WAKE: RefCell<Option<WakeHandle>> = const { RefCell::new(None) };
    /// Root context handle; child → parent messages queue a delivery job here.
    static ROOT_CTX: RefCell<Option<Context<'static>>> = const { RefCell::new(None) };
    /// Per-run suffix for the child global rooting a pending completion promise.
    static RUN_SERIAL: Cell<u32> = const { Cell::new(1) };
    /// Depth of the import-policy call in progress.
    ///
    /// A policy runs inside the runtime tree's tick, so creating or closing a
    /// runtime from it would re-enter the tree — the policy must not.
    static IMPORT_POLICY_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Set when a create/close was refused because it happened inside an
    /// import policy. The policy's own exception then has to be reported as
    /// the re-entrancy error, not as a generic "the policy threw".
    static IMPORT_POLICY_REENTRY: Cell<bool> = const { Cell::new(false) };
}

/// Remember the root runtime, park wake handle and context (called once, on the
/// root).
pub(crate) fn set_root_runtime(rt: Runtime) {
    let wake = rt.wake_handle();
    ROOT_RUNTIME.with(|r| *r.borrow_mut() = Some(rt));
    ROOT_WAKE.with(|w| *w.borrow_mut() = Some(wake));
}

/// Remember the root context handle.
pub(crate) fn set_root_ctx(ctx: Context<'static>) {
    ROOT_CTX.with(|c| *c.borrow_mut() = Some(ctx));
}

fn root_runtime() -> Option<Runtime> {
    ROOT_RUNTIME.with(|r| r.borrow().clone())
}

/// The root context, as a copy: the context handle is `Copy`, so a caller can
/// take one without borrowing the registry.
fn root_context() -> Option<Context<'static>> {
    ROOT_CTX.with(|c| *c.borrow())
}

fn wake_root() {
    ROOT_WAKE.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            w.wake();
        }
    });
}

fn get_child(id: u32) -> Option<Rc<Child>> {
    CHILDREN.with(|m| m.borrow().get(&id).cloned())
}

fn remove_child(id: u32) -> Option<Rc<Child>> {
    CHILDREN.with(|m| m.borrow_mut().remove(&id))
}

/// Install the per-runtime GC trace handler. Must be called for the root and
/// for every child: each handler traces only the state belonging to `rt`'s id,
/// plus (root only) process-global / not-yet-per-runtime state and the
/// parent-heap child handles.
pub(crate) fn register_runtime_traces(rt: &mut Runtime) {
    if rt.is_root() {
        rt.gc_set_trace_handler(move |gcx| {
            // Per-runtime state is traced via `AikeRuntimeState::trace`; only the
            // root's child-runtime handles need a trace handler here.
            gc_trace_runtime_handles(gcx);
        });
    }
}

/// Gate a process/thread-global capability: reject when running inside a child
/// runtime (`Aike.createRuntime`). Used by features that must not be invoked
/// from a child (process exit, signals, terminal control, ...).
pub(crate) fn ensure_root_runtime(ctx: &mut ScopedContext, feature: &str) -> Result<(), i32> {
    if ctx.rt().is_root() {
        Ok(())
    } else {
        Err(ctx.throw_type_error(&format!("{feature} is only available in the root runtime")))
    }
}

/// keep every child runtime handle object alive.
pub(crate) fn gc_trace_runtime_handles(gcx: &mut GcTraceCtx) {
    CHILDREN.with(|m| {
        for c in m.borrow().values() {
            c.handle.trace(gcx);
            // The import policy is a parent-heap value: the root's heap must
            // keep it alive for as long as the child exists.
            if let Some(p) = &c.import_policy {
                p.trace(gcx);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// strict option parsing
// ---------------------------------------------------------------------------

fn read_string(ctx: &mut ScopedContext, v: JSValue, name: &str) -> Result<String, i32> {
    match v.as_string(false) {
        Some(s) => Ok(s.content().encode_utf8(false).to_string()),
        None => Err(ctx.throw_type_error(&format!("`{name}` must be a string"))),
    }
}

/// Non-negative integer (0 allowed).
fn read_uint(ctx: &mut ScopedContext, v: JSValue, name: &str) -> Result<u64, i32> {
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`{name}` must be a number")));
    }
    match v.to_number(ctx)? {
        zuqe::IntOrFloat::Int64(i) if i >= 0 => Ok(i as u64),
        zuqe::IntOrFloat::Float64(d) if d >= 0.0 && d.fract() == 0.0 => Ok(d as u64),
        _ => Err(ctx.throw_range_error(&format!("`{name}` must be a non-negative integer"))),
    }
}

fn read_string_array(ctx: &mut ScopedContext, v: JSValue, name: &str) -> Result<Vec<String>, i32> {
    let arr = Array::try_from(v)
        .map_err(|_| ctx.throw_type_error(&format!("`{name}` must be an array")))?;
    let len = arr.get_length_prop(ctx)? as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let item = arr.get_property_u32(ctx, i as u32)?;
        out.push(read_string(ctx, item, name)?);
    }
    Ok(out)
}

/// Supported host **global** names.
///
/// The capability list only governs globals. Modules are not capabilities:
/// whether a runtime may import a module is decided by the import policy
/// (`Aike.createRuntime({ importPolicy })`).
const ALL_CAPS: &[&str] = &[
    "console",
    "crypto",
    "fetch",
    "url",
    "timers",
    "microtask",
    "abort",
    "streams",
    "blob",
    "formData",
    "textEncoder",
    "textDecoder",
    "dom",
    "base64",
    "structuredClone",
    "process",
    "Aike",
];

/// Capabilities that may only be installed in the root runtime (they back onto
/// process/thread-global state). Requesting one for a child is a TypeError.
const ROOT_ONLY_CAPS: &[&str] = &[];

struct RuntimeOpts {
    memory: Option<u64>,
    timeout: Option<u64>,
    /// `None` = explicit blacklist mode (install all except `black_list`);
    /// `Some(list)` = whitelist mode (install exactly `list`).
    white_list: Option<Vec<String>>,
    black_list: Vec<String>,
    name: Option<String>,
    /// The import policy: a **parent-heap** function consulted before every
    /// module load. `None` = no policy given, which denies every import.
    import_policy: Option<JSValue>,
}

fn parse_opts(ctx: &mut ScopedContext, v: JSValue) -> Result<RuntimeOpts, i32> {
    if v.is_undefined() {
        return Ok(RuntimeOpts {
            memory: None,
            timeout: None,
            white_list: Some(Vec::new()),
            black_list: Vec::new(),
            name: None,
            import_policy: None,
        });
    }
    let Some(opts) = v.as_object() else {
        return Err(
            ctx.throw_type_error("`options` must be an object ({memory, timeout, global, name})")
        );
    };

    let memory = match opts.get_property_str(ctx, "memory")? {
        x if x.is_undefined() => None,
        x => Some(read_uint(ctx, x, "memory")?),
    };
    let timeout = match opts.get_property_str(ctx, "timeout")? {
        x if x.is_undefined() => None,
        x => Some(read_uint(ctx, x, "timeout")?),
    };
    let name = match opts.get_property_str(ctx, "name")? {
        x if x.is_undefined() => None,
        x => Some(read_string(ctx, x, "name")?),
    };
    let import_policy = match opts.get_property_str(ctx, "importPolicy")? {
        x if x.is_undefined() => None,
        x if x.is_callable() => Some(x),
        _ => return Err(ctx.throw_type_error("`importPolicy` must be a function")),
    };

    let g = opts.get_property_str(ctx, "global")?;
    let (white_list, black_list) = if g.is_undefined() {
        // fail-safe default: pure ECMA, no host capability.
        (Some(Vec::new()), Vec::new())
    } else if g.is_array(ctx)? {
        (Some(read_string_array(ctx, g, "global")?), Vec::new())
    } else if let Some(go) = g.as_object() {
        let wl = go.get_property_str(ctx, "whiteList")?;
        let bl = go.get_property_str(ctx, "blackList")?;
        if !wl.is_undefined() {
            // whiteList wins; blackList is ignored.
            (
                Some(read_string_array(ctx, wl, "global.whiteList")?),
                Vec::new(),
            )
        } else if !bl.is_undefined() {
            (None, read_string_array(ctx, bl, "global.blackList")?)
        } else {
            // `{}` → no whiteList: blacklist mode with an empty list (fail-open).
            (None, Vec::new())
        }
    } else {
        return Err(
            ctx.throw_type_error("`global` must be an array of names or {whiteList?, blackList?}")
        );
    };

    Ok(RuntimeOpts {
        memory,
        timeout,
        white_list,
        black_list,
        name,
        import_policy,
    })
}

fn validate_cap(ctx: &mut ScopedContext, name: &str) -> Result<(), i32> {
    if ALL_CAPS.contains(&name) {
        Ok(())
    } else {
        Err(ctx.throw_type_error(&format!("unknown host capability `{name}`")))
    }
}

// ---------------------------------------------------------------------------
// Child context construction
// ---------------------------------------------------------------------------

fn stream_classes() -> Vec<ClassID> {
    vec![
        stream::JSReadableStream::class_id(),
        stream::JSReadableStreamDefaultReader::class_id(),
        stream::JSReadableStreamDefaultController::class_id(),
        stream::JSReadableByteStreamController::class_id(),
        stream::JSReadableStreamBYOBRequest::class_id(),
        stream::JSWritableStream::class_id(),
        stream::JSWritableStreamDefaultWriter::class_id(),
        stream::JSWritableStreamDefaultController::class_id(),
        stream::JSTransformStream::class_id(),
        stream::JSTransformStreamDefaultController::class_id(),
        stream::JSByteLengthQueuingStrategy::class_id(),
        stream::JSCountQueuingStrategy::class_id(),
    ]
}

/// Classes a capability needs initialized in the child context. `use_classes`
/// is idempotent, so overlaps with installers that also init classes are safe.
fn cap_classes(cap: &str) -> Vec<ClassID> {
    match cap {
        "console" | "timers" | "microtask" | "base64" => Vec::new(),
        "crypto" => vec![
            crypto::JSCrypto::class_id(),
            crypto::subtle::JSCryptoSubtle::class_id(),
            crypto::JSCryptoKey::class_id(),
        ],
        "url" => vec![url::JSURL::class_id(), url::JSURLSearchParams::class_id()],
        "dom" | "structuredClone" => vec![dom::JSDOMException::class_id()],
        "textEncoder" => vec![text_encoder::JSTextEncoder::class_id()],
        "textDecoder" => vec![text_decoder::JSTextDecoder::class_id()],
        "abort" => vec![
            abort::JSAbortController::class_id(),
            abort::JSAbortSignal::class_id(),
        ],
        "streams" => stream_classes(),
        "blob" => vec![blob::JSBlob::class_id()],
        "formData" => vec![
            form_data::JSFormData::class_id(),
            form_data::FormDataIterator::class_id(),
        ],
        "fetch" => {
            let mut v = cap_classes("abort");
            v.extend(cap_classes("streams"));
            v.extend(cap_classes("blob"));
            v.extend(cap_classes("formData"));
            v.extend(cap_classes("url"));
            v.extend(cap_classes("dom"));
            v.push(text_encoder::JSTextEncoder::class_id());
            v.push(text_decoder::JSTextDecoder::class_id());
            v
        }
        "process" => vec![crate::class_event_emitter::JSEventEmitter::class_id()],
        "Aike" => vec![
            super::JSCronJob::class_id(),
            crate::class_event_emitter::JSEventEmitter::class_id(),
            super::JSStdioHandle::class_id(),
            super::JSFileHandle::class_id(),
        ],
        _ => Vec::new(),
    }
}

/// Install one capability's globals into a child context. Class
/// initialization is done separately (see `cap_classes`).
fn install_cap(cx: &mut ScopedContext, cap: &str) -> Result<(), i32> {
    let mut global = cx.global_obj();
    match cap {
        "console" => console::init_console(cx, &mut global)?,
        "timers" => timeout::init_timeout(cx, global)?,
        "microtask" => microtask::init_microtask(cx, global)?,
        "base64" => base64::init_encoding(cx, global)?,
        "structuredClone" => structured_clone::init_structured_clone(cx, global)?,
        "crypto" => crypto::init_crypto(cx, global)?,
        "url" => url::init_url(cx, &mut global)?,
        "abort" => abort::init_abort_signal_statics(cx, &mut global)?,
        "streams" => stream::init_readable_stream_statics(cx, &mut global)?,
        "fetch" => fetch::init_fetch(cx, global)?,
        "process" => {
            let (argv, exec_argv, exec_path) = PROCESS_INFO.with(|p| p.borrow().clone());
            crate::process::init_process(cx, &argv, &exec_argv, &exec_path)?;
        }
        "Aike" => super::init_aike_namespace(cx)?,
        // Capabilities without an installer are pure class exposures
        // (`dom`, `blob`, `formData`, `textEncoder`, `textDecoder`); their
        // constructors were exposed by `use_classes`.
        _ => {}
    }

    Ok(())
}

/// Build a child context: pure ECMA + the symmetric messaging globals + the
/// whitelisted capabilities. `eval` is always stripped; `Aike.createRuntime`,
/// `Aike.evalScript` and `Aike.evalModule` are never installed.
fn setup_child(cx: &mut ScopedContext, id: u32, caps: &[String]) -> Result<(), i32> {
    ScopedContext::ECMA_CONTEXT_SETUP(cx)?;

    // Hard-strip `eval`: it must not be an entry point into the child.
    let mut g: JSValue = cx.global_obj().into();
    let _ = g.delete_property_str(cx, "eval");

    // Symmetric messaging globals (always present).
    let pm = cx.create_native_function(
        Callback::Func(js_child_post_message),
        "postMessage",
        1,
        id as i16,
    )?;
    cx.global_obj()
        .define_property_value_str(cx, "postMessage", pm.into(), JS_PROP_C_W_E)?;
    cx.global_obj().define_property_value_str(
        cx,
        "onmessage",
        JSValue::UNDEFINED,
        JS_PROP_C_W_E,
    )?;

    // Initialize exactly the classes the requested capabilities need, once.
    let mut ids: Vec<ClassID> = Vec::new();
    for cap in caps {
        for cid in cap_classes(cap) {
            if !ids.contains(&cid) {
                ids.push(cid);
            }
        }
    }
    if !ids.is_empty() {
        cx.use_classes(&ids)?;
    }
    for cap in caps {
        install_cap(cx, cap)?;
    }
    Ok(())
}

/// Queue a one-shot job on `ctx`, holding `child` **weakly**.
///
/// The job lives in `ctx`'s runtime heap. A strong capture of the child would
/// be a cycle — job → child → runtime → heap → job — and the runtime could
/// never be freed, not even by `close()`.
fn queue_oneshot_holding<F>(mut ctx: Context<'static>, child: &Rc<Child>, f: F)
where
    F: Fn(&mut ScopedContext, &Rc<Child>) -> Result<(), i32> + 'static,
{
    let weak = Rc::downgrade(child);
    ctx.queue_oneshot_job(
        move |cx| match weak.upgrade() {
            Some(child) => f(cx, &child),
            None => Ok(()),
        },
        (),
    );
}

/// Queue a polling job on `ctx`, holding `child` **weakly**.
///
/// See [`queue_oneshot_holding`] for why the capture must be weak.
fn queue_polling_holding<F>(mut ctx: Context<'static>, child: &Rc<Child>, f: F)
where
    F: Fn(&mut ScopedContext, &Rc<Child>) -> Result<JobState, i32> + 'static,
{
    let weak = Rc::downgrade(child);
    ctx.queue_job(
        move |cx| match weak.upgrade() {
            Some(child) => f(cx, &child),
            None => Ok(JobState::Done),
        },
        (),
    );
}

/// Queue a one-shot job on the root context, holding `child` **weakly**.
///
/// The child holds its parent, so a strong capture here is a cycle through
/// the *root's* heap: job → child → parent → root's heap → job.
fn queue_root_job_holding<F>(child: &Rc<Child>, f: F)
where
    F: Fn(&mut ScopedContext, &Rc<Child>) -> Result<(), i32> + 'static,
{
    if let Some(mut ctx) = ROOT_CTX.with(|c| *c.borrow()) {
        queue_oneshot_holding(ctx, child, f);
    }
}

// ---------------------------------------------------------------------------
// import policy
// ---------------------------------------------------------------------------

/// Whether an import policy is currently being evaluated.
///
/// A policy runs inside the runtime tree's tick: creating or closing a
/// runtime from it would re-enter the tree, so both are refused.
fn in_import_policy() -> bool {
    IMPORT_POLICY_DEPTH.with(|d| d.get() > 0)
}

/// Ask the runtime's import policy whether `name` may be loaded.
///
/// `name` is the module's name for a module (`"ai"`, `"sqlite"`) and the
/// absolute path for a script file. The root runtime has no policy and
/// allows everything.
///
/// Errors are reported in the importing runtime's context, so the import
/// fails where it was attempted:
/// - no policy given → every import is denied;
/// - the policy returned `false` → denied;
/// - the policy threw → its message is reported;
/// - the policy returned a non-boolean → a protocol error.
pub(crate) fn ensure_import_allowed(ctx: &mut ScopedContext, name: &str) -> Result<(), i32> {
    // The root runtime is unrestricted.
    let Some(id) = ctx.rt().user_state::<super::AikeRuntimeState>().child_id else {
        return Ok(());
    };
    let Some(child) = get_child(id) else {
        return Ok(());
    };
    let Some(policy) = child.import_policy else {
        return Err(ctx.throw_reference_error(&format!(
            "import of '{name}' is not allowed (no importPolicy)"
        )));
    };

    // The policy is a parent-heap value: its argument must live in the same
    // heap, and the call must run in the parent's context.
    let Some(mut parent) = root_context() else {
        return Err(ctx.throw_internal_error("no root context for the import policy"));
    };
    let arg = parent.with_root_scope(|mut cx| {
        cx.create_string_from_utf8(name.as_bytes().to_vec())
            .map(JSValue::make_string)
    })?;

    let result = {
        IMPORT_POLICY_REENTRY.with(|f| f.set(false));
        IMPORT_POLICY_DEPTH.with(|d| d.set(d.get() + 1));
        let r = Context::call_in_ctx(&parent, policy, &[arg]);
        IMPORT_POLICY_DEPTH.with(|d| d.set(d.get() - 1));
        r
    };

    // A create/close from inside the policy is a re-entrancy error, not an
    // ordinary policy failure: report it as such.
    if IMPORT_POLICY_REENTRY.with(|f| f.get()) {
        return Err(ctx.throw_type_error("importPolicy must not create or close runtimes"));
    }

    match result {
        Ok(v) => {
            if !v.is_bool() {
                return Err(ctx.throw_reference_error("importPolicy must return a boolean"));
            }
            if v.as_bool() == Some(false) {
                return Err(ctx.throw_reference_error(&format!(
                    "import of '{name}' is denied by the runtime's importPolicy"
                )));
            }
            Ok(())
        }
        Err(_) => {
            // The policy threw: report its message, never swallow it.
            let text = parent.take_exception_as_text();
            Err(ctx.throw_reference_error(&format!("importPolicy threw: {}", text.message)))
        }
    }
}

// ---------------------------------------------------------------------------
// Aike.createRuntime
// ---------------------------------------------------------------------------

pub(crate) fn js_create_runtime(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // A policy must not re-enter the runtime tree.
    if in_import_policy() {
        IMPORT_POLICY_REENTRY.with(|f| f.set(true));
        return Err(ctx.throw_type_error("importPolicy must not create or close runtimes"));
    }

    let opts = parse_opts(ctx, cc.arg(0))?;

    // Resolve the effective capability list, validating whitelist names.
    let caps: Vec<String> = match &opts.white_list {
        Some(wl) => {
            for name in wl {
                validate_cap(ctx, name)?;
            }
            wl.clone()
        }
        None => {
            let bl: HashSet<&str> = opts.black_list.iter().map(|s| s.as_str()).collect();
            ALL_CAPS
                .iter()
                .filter(|c| !bl.contains(**c) && !ROOT_ONLY_CAPS.contains(*c))
                .map(|c| c.to_string())
                .collect()
        }
    };

    // A child runtime must not request root-only capabilities. Note the
    // blacklist path (None) also installs all caps, so filter those out here.
    for cap in &caps {
        if ROOT_ONLY_CAPS.contains(&cap.as_str()) {
            return Err(ctx.throw_type_error(&format!(
                "host capability `{cap}` is only available in the root runtime"
            )));
        }
    }

    let id = NEXT_ID.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    });
    if id > i16::MAX as u32 {
        return Err(ctx.throw_range_error("too many runtimes created"));
    }

    let Some(root) = root_runtime() else {
        return Err(ctx.throw_internal_error("no root runtime"));
    };
    let mut rt = Runtime::create_child_with(&root, super::AikeRuntimeState::new())
        .map_err(|_| ctx.throw_internal_error("cannot create runtime"))?;
    if let Some(mem) = opts.memory
        && mem > 0
    {
        // ponytail: best-effort — maps to the engine GC limit, which the
        // current build does not enforce for arena/large allocations. Callers
        // relying on hard memory isolation need engine-side enforcement.
        rt.gc_set_memory_limit(mem as usize);
    }

    // The child resolves and loads modules like the root does — but the
    // loader consults the import policy before it installs anything.
    rt.set_module_normalizer(crate::module_loader::js_module_normalizer);
    rt.set_module_loader(crate::module_loader::js_module_loader);
    rt.user_state_mut::<super::AikeRuntimeState>().child_id = Some(id);

    // Interrupt handler: abort synchronous overruns at the run deadline. (The
    // child shares the root's park, so no per-child deadline waker is needed.)
    let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    {
        let dl = deadline.clone();
        rt.set_interrupt_handler(move |_rt| dl.get().is_some_and(|d| Instant::now() >= d));
    }

    // The child's GC traces only state belonging to this runtime.
    register_runtime_traces(&mut rt);

    let caps_for_setup = caps.clone();
    let child_ctx = rt
        .create_context(move |cx| setup_child(cx, id, &caps_for_setup))
        .map_err(|_| ctx.throw_internal_error("failed to initialize runtime context"))?;

    // Build the parent-side handle object.
    let mut handle = ctx.create_base_object()?;
    let run_fn = ctx.create_native_function(Callback::Func(js_run), "run", 2, id as i16)?;
    handle.define_property_value_str(ctx, "run", run_fn.into(), JS_PROP_C_W_E)?;
    let post_fn = ctx.create_native_function(
        Callback::Func(js_parent_post_message),
        "postMessage",
        1,
        id as i16,
    )?;
    handle.define_property_value_str(ctx, "postMessage", post_fn.into(), JS_PROP_C_W_E)?;
    let close_fn = ctx.create_native_function(Callback::Func(js_close), "close", 0, id as i16)?;
    handle.define_property_value_str(ctx, "close", close_fn.into(), JS_PROP_C_W_E)?;
    handle.define_property_value_str(ctx, "onmessage", JSValue::NULL, JS_PROP_C_W_E)?;
    if let Some(name) = &opts.name {
        let s = ctx
            .create_string_from_utf8(name.clone().into_bytes())
            .map(JSValue::make_string)?;
        handle.define_property_value_str(ctx, "name", s, JS_PROP_ENUMERABLE)?;
    }
    let handle_val: JSValue = handle.into();

    let child = Rc::new(Child {
        rt,
        ctx: child_ctx,
        import_policy: opts.import_policy,
        default_timeout: opts.timeout,
        handle: handle_val,
        deadline,
        in_msg: RefCell::new(VecDeque::new()),
        out_msg: RefCell::new(VecDeque::new()),
        run_slots: RefCell::new(Vec::new()),
        closed: Cell::new(false),
    });
    CHILDREN.with(|m| m.borrow_mut().insert(id, child));

    Ok(handle_val)
}

// ---------------------------------------------------------------------------
// handle methods
// ---------------------------------------------------------------------------

fn js_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id = cc.magic() as u32;
    let Some(child) = get_child(id) else {
        return Err(ctx.throw_type_error("runtime has been closed"));
    };
    if child.closed.get() {
        return Err(ctx.throw_type_error("runtime has been closed"));
    }

    let code = read_string(ctx, cc.arg(0), "code")?;
    let name = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        read_string(ctx, cc.arg(1), "name")?
    } else {
        format!("<runtime-{id}>")
    };

    let slot: PromiseResultSlot<RunOutcome> = PromiseResultSlot::new();
    let promise = Promise::create_with_result_slot(ctx, slot.clone(), make_run_promise)?;
    let shared: SharedSlot = Rc::new(RefCell::new(Some(slot)));
    child.run_slots.borrow_mut().push(shared.clone());

    // Schedule the eval as a polling job on the child; the child's persistent
    // driver runs it.
    let started = Cell::new(false);
    let key = format!(
        "{RESULT_KEY_PREFIX}{}__",
        RUN_SERIAL.with(|n| {
            let v = n.get();
            n.set(v + 1);
            v
        })
    );
    queue_polling_holding(child.ctx, &child, move |cx, child| {
        run_job(cx, child, &started, &code, &name, &key, &shared)
    });
    child.rt.wake_handle().wake();

    Ok(promise.into())
}

/// One scheduled `run()`: eval the code once, then poll the completion promise
/// until it settles (or the run deadline passes), and commit the outcome.
fn run_job(
    cx: &mut ScopedContext,
    child: &Rc<Child>,
    started: &Cell<bool>,
    code: &str,
    name: &str,
    key: &str,
    shared: &SharedSlot,
) -> Result<JobState, i32> {
    // Timeout also covers async waits: check before anything else once running.
    if started.get()
        && let Some(d) = child.deadline.get()
        && Instant::now() >= d
    {
        child.deadline.set(None);
        commit_timeout(shared);
        return Ok(JobState::Done);
    }

    if !started.get() {
        started.set(true);
        child.deadline.set(
            child
                .default_timeout
                .filter(|t| *t > 0)
                .map(|t| Instant::now() + Duration::from_millis(t)),
        );
        let mut rt = child.rt.clone();
        let _ = rt.take_interrupt();
        let _ = cx.take_exception();

        return match cx.eval(
            code.as_bytes(),
            &EvalName::Name(name.to_string()),
            EvalType::Global,
            EvalFlags::empty(),
        ) {
            Err(_) => {
                if rt.take_interrupt().is_some() {
                    child.deadline.set(None);
                    commit_timeout(shared);
                } else {
                    let exc = cx.take_exception();
                    child.deadline.set(None);
                    commit_error(cx, shared, exc);
                }
                Ok(JobState::Done)
            }
            Ok(v) => {
                if rt.take_interrupt().is_some() {
                    child.deadline.set(None);
                    commit_timeout(shared);
                    return Ok(JobState::Done);
                }
                match Promise::try_from(v) {
                    Ok(p) if p.state() == PromiseState::Pending => {
                        // Root the completion promise and keep polling.
                        if cx.global_obj().set_property_str(cx, key, v).is_err() {
                            let _ = cx.take_exception();
                            child.deadline.set(None);
                            commit_timeout(shared);
                            return Ok(JobState::Done);
                        }
                        if let Some(d) = child.deadline.get() {
                            cx.set_poll_hint(d);
                        }
                        Ok(JobState::Block)
                    }
                    Ok(p) => {
                        child.deadline.set(None);
                        if p.state() == PromiseState::Rejected {
                            let reason = p.result();
                            commit_rejection(cx, shared, reason);
                        } else {
                            commit_value(cx, shared, p.result());
                        }
                        Ok(JobState::Done)
                    }
                    Err(_) => {
                        child.deadline.set(None);
                        commit_value(cx, shared, v);
                        Ok(JobState::Done)
                    }
                }
            }
        };
    }

    // Re-check the rooted completion promise.
    let v = match cx.global_obj().get_property_str(cx, key) {
        Ok(v) => v,
        Err(_) => {
            let _ = cx.take_exception();
            return Ok(JobState::Done);
        }
    };
    match Promise::try_from(v) {
        Ok(p) => match p.state() {
            PromiseState::Fulfilled => {
                let r = p.result();
                let _ = cx.global_obj().delete_property_str(cx, key);
                child.deadline.set(None);
                commit_value(cx, shared, r);
                Ok(JobState::Done)
            }
            PromiseState::Rejected => {
                let r = p.result();
                let _ = cx.global_obj().delete_property_str(cx, key);
                child.deadline.set(None);
                commit_rejection(cx, shared, r);
                Ok(JobState::Done)
            }
            PromiseState::Pending => {
                if let Some(d) = child.deadline.get() {
                    cx.set_poll_hint(d);
                }
                Ok(JobState::Block)
            }
        },
        Err(_) => Ok(JobState::Done),
    }
}

fn take_slot(shared: &SharedSlot) -> Option<PromiseResultSlot<RunOutcome>> {
    shared.borrow_mut().take()
}

fn commit_value(cx: &mut ScopedContext, shared: &SharedSlot, val: JSValue) {
    let Some(slot) = take_slot(shared) else {
        return;
    };
    let outcome = match JSValue::dump(cx, val) {
        Ok(blob) => RunOutcome::Value(blob.to_vec()),
        Err(e) => RunOutcome::Error {
            name: "TypeError".into(),
            message: format!("runtime.run result is not serializable: {e}"),
            stack: None,
        },
    };
    let _ = slot.commit(outcome);
}

fn commit_error(cx: &mut ScopedContext, shared: &SharedSlot, exc: Exception) {
    let Some(slot) = take_slot(shared) else {
        return;
    };
    let outcome = match exc {
        Exception::OutOfMemory => RunOutcome::Error {
            name: "MemoryLimitError".into(),
            message: "runtime memory limit exceeded".into(),
            stack: None,
        },
        Exception::Error(v) => error_outcome_from_value(cx, v, "Error"),
        _ => {
            let text = exc.to_text(cx);
            RunOutcome::Error {
                name: "Error".into(),
                message: text.message,
                stack: text.traceback,
            }
        }
    };
    let _ = slot.commit(outcome);
}

fn commit_rejection(cx: &mut ScopedContext, shared: &SharedSlot, reason: JSValue) {
    let Some(slot) = take_slot(shared) else {
        return;
    };
    let outcome = error_outcome_from_value(cx, reason, "Error");
    let _ = slot.commit(outcome);
}

fn commit_timeout(shared: &SharedSlot) {
    let Some(slot) = take_slot(shared) else {
        return;
    };
    let _ = slot.commit(RunOutcome::Error {
        name: "TimeoutError".into(),
        message: "runtime.run timed out".into(),
        stack: None,
    });
}

fn js_parent_post_message(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id = cc.magic() as u32;
    let Some(child) = get_child(id) else {
        return Err(ctx.throw_type_error("runtime has been closed"));
    };
    if child.closed.get() {
        return Err(ctx.throw_type_error("runtime has been closed"));
    }
    let val = cc.arg(0);
    ensure_serializable(ctx, val)?;
    let blob =
        JSValue::dump(ctx, val).map_err(|e| ctx.throw_type_error(&format!("postMessage: {e}")))?;
    child.in_msg.borrow_mut().push_back(blob.to_vec());

    // Deliver on the child's own runtime (its persistent driver runs the job).
    queue_oneshot_holding(child.ctx, &child, |cx, child| {
        deliver_to_child(cx, child);
        Ok(())
    });
    child.rt.wake_handle().wake();
    Ok(JSValue::UNDEFINED)
}

fn js_child_post_message(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id = cc.magic() as u32;
    let val = cc.arg(0);
    ensure_serializable(ctx, val)?;
    let blob =
        JSValue::dump(ctx, val).map_err(|e| ctx.throw_type_error(&format!("postMessage: {e}")))?;
    let Some(child) = get_child(id) else {
        return Ok(JSValue::UNDEFINED);
    };
    child.out_msg.borrow_mut().push_back(blob.to_vec());

    // Deliver on the root runtime: queue a one-shot job there and wake it.
    queue_root_job_holding(&child, |cx, child| {
        deliver_to_parent(cx, child);
        Ok(())
    });
    wake_root();
    Ok(JSValue::UNDEFINED)
}

fn js_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // A policy must not re-enter the runtime tree.
    if in_import_policy() {
        IMPORT_POLICY_REENTRY.with(|f| f.set(true));
        return Err(ctx.throw_type_error("importPolicy must not create or close runtimes"));
    }
    let id = cc.magic() as u32;
    let Some(child) = remove_child(id) else {
        return Ok(JSValue::UNDEFINED);
    };
    child.closed.set(true);
    // Reject in-flight runs (their jobs are dropped with the runtime).
    for shared in child.run_slots.borrow().iter() {
        if let Some(slot) = shared.borrow_mut().take() {
            let _ = slot.commit(RunOutcome::Error {
                name: "Error".into(),
                message: "runtime is closed".into(),
                stack: None,
            });
        }
    }
    // Detach from the engine tree's tick list (the child dies with its
    // handle; the parent only stops driving it).
    if let Some(root) = root_runtime() {
        root.remove_child(&child.rt);
    }
    wake_root();
    Ok(JSValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// message delivery (runs inside per-runtime jobs)
// ---------------------------------------------------------------------------

/// Build an error outcome from a JS value (an Error or any thrown value).
fn error_outcome_from_value(cx: &mut ScopedContext, v: JSValue, default_name: &str) -> RunOutcome {
    let name = v
        .get_property_str(cx, "name")
        .ok()
        .and_then(|n| n.as_string(false))
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|| default_name.to_string());
    let message = v
        .to_string(cx)
        .ok()
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let stack = v
        .get_property_str(cx, "stack")
        .ok()
        .and_then(|s| s.as_string(false))
        .map(|s| s.content().encode_utf8(false).to_string())
        .filter(|s| !s.is_empty());
    // Clear any exception raised while probing properties.
    let _ = cx.take_exception();
    RunOutcome::Error {
        name,
        message,
        stack,
    }
}

// ---------------------------------------------------------------------------
// message delivery (runs inside per-runtime one-shot jobs)
// ---------------------------------------------------------------------------

/// Deliver queued parent → child messages to the child's `onmessage`, running
/// inside a job on the child's own context (`cx` is the child's `ScopedContext`).
fn deliver_to_child(cx: &mut ScopedContext, child: &Rc<Child>) -> bool {
    if child.in_msg.borrow().is_empty() {
        return false;
    }
    let handler = match cx.global_obj().get_property_str(cx, "onmessage") {
        Ok(h) => h,
        Err(_) => {
            let _ = cx.take_exception();
            return false;
        }
    };
    if !handler.is_callable() {
        return false;
    }
    let mut did = false;
    loop {
        let Some(blob) = child.in_msg.borrow_mut().pop_front() else {
            break;
        };
        let Ok(val) = JSValue::restore(cx, &blob) else {
            let _ = cx.take_exception();
            continue;
        };
        if handler.call(cx, JSValue::UNDEFINED, &[val]).is_err() {
            let _ = cx.take_exception();
        }
        did = true;
    }
    did
}

fn deliver_to_parent(parent_ctx: &mut ScopedContext, child: &Rc<Child>) -> bool {
    if child.out_msg.borrow().is_empty() {
        return false;
    }
    let handler = match child.handle.get_property_str(parent_ctx, "onmessage") {
        Ok(h) => h,
        Err(_) => {
            let _ = parent_ctx.take_exception();
            return false;
        }
    };
    if !handler.is_callable() {
        return false;
    }
    let mut did = false;
    loop {
        let blob = match child.out_msg.borrow_mut().pop_front() {
            Some(b) => b,
            None => break,
        };
        let val = match JSValue::restore(parent_ctx, &blob) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if handler
            .call(parent_ctx, JSValue::UNDEFINED, &[val])
            .is_err()
        {
            let _ = parent_ctx.take_exception();
        }
        did = true;
    }
    did
}

// ---------------------------------------------------------------------------
// serializability check (structured-clone-ish), used before dump
// ---------------------------------------------------------------------------

fn ensure_serializable(ctx: &mut ScopedContext, v: JSValue) -> Result<(), i32> {
    let mut seen: HashSet<JSValue> = HashSet::new();
    check_serializable(ctx, v, &mut seen)
}

fn check_serializable(
    ctx: &mut ScopedContext,
    v: JSValue,
    seen: &mut HashSet<JSValue>,
) -> Result<(), i32> {
    if v.is_symbol() {
        return Err(ctx.throw_type_error("postMessage: a Symbol cannot be sent between runtimes"));
    }
    if v.is_callable() {
        return Err(ctx.throw_type_error("postMessage: a function cannot be sent between runtimes"));
    }
    if !v.is_object() {
        return Ok(());
    }
    if !seen.insert(v) {
        return Ok(());
    }
    let mut obj = v
        .as_object()
        .ok_or_else(|| ctx.throw_internal_error("not an object"))?;

    if v.as_object_ArrayBuffer().is_some()
        || v.as_object_TypedArray().is_some()
        || v.as_object_DataView().is_some()
    {
        return Ok(());
    }

    let cid = obj.class_id();
    if cid == JS_CLASS_PROMISE {
        return Err(ctx.throw_type_error("postMessage: a Promise cannot be sent between runtimes"));
    }
    if cid == JS_CLASS_WEAKMAP || cid == JS_CLASS_WEAKSET {
        return Err(
            ctx.throw_type_error("postMessage: a WeakMap/WeakSet cannot be sent between runtimes")
        );
    }

    if cid == JS_CLASS_MAP || cid == JS_CLASS_SET {
        if let Some(ms) = obj.payload_mapset() {
            for e in ms.iter() {
                check_serializable(ctx, e.key(), seen)?;
                if cid == JS_CLASS_MAP {
                    check_serializable(ctx, e.value(), seen)?;
                }
            }
        }
        return Ok(());
    }

    // Recurse only into plain containers; exotic/native objects are left for
    // `dump` to reject.
    if matches!(
        cid,
        JS_CLASS_OBJECT
            | JS_CLASS_ARRAY
            | JS_CLASS_ERROR
            | JS_CLASS_BOOLEAN
            | JS_CLASS_NUMBER
            | JS_CLASS_STRING
    ) {
        let keys = obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
        for pk in keys.iter() {
            let val = obj.get_property(ctx, pk.atom)?;
            check_serializable(ctx, val, seen)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child runtime, built the way `js_create_runtime` builds one.
    fn make_child(root: &Runtime) -> Result<Rc<Child>, i32> {
        let mut rt = Runtime::create_child(root)?;
        let ctx: Context<'static> = rt.create_context(ScopedContext::ECMA_CONTEXT_SETUP)?;
        Ok(Rc::new(Child {
            rt,
            ctx,
            import_policy: None,
            default_timeout: None,
            handle: JSValue::UNDEFINED,
            deadline: Rc::new(Cell::new(None)),
            in_msg: RefCell::new(VecDeque::new()),
            out_msg: RefCell::new(VecDeque::new()),
            run_slots: RefCell::new(Vec::new()),
            closed: Cell::new(false),
        }))
    }

    /// A job queued on a runtime's **own** context must hold the child
    /// weakly: the job lives in the child's heap, so a strong capture is a
    /// cycle — job → child → runtime → heap → job — and the runtime could
    /// never be freed, not even by `close()`.
    #[test]
    fn a_queued_job_does_not_keep_its_runtime_alive() -> Result<(), i32> {
        let root = Runtime::create()?;
        let child = make_child(&root)?;

        queue_polling_holding(child.ctx, &child, |_cx, _child| Ok(JobState::Done));

        let weak = Rc::downgrade(&child);
        drop(child);
        assert!(
            weak.upgrade().is_none(),
            "a queued job must not keep its runtime alive"
        );
        Ok(())
    }

    /// The same holds for a job on the **parent's** context: the child holds
    /// its parent, so a strong capture there is a cycle through the parent's
    /// heap (job → child → parent → parent's heap → job).
    #[test]
    fn a_root_job_does_not_keep_the_child_alive() -> Result<(), i32> {
        let mut root = Runtime::create()?;
        let root_ctx: Context<'static> = root.create_context(ScopedContext::ECMA_CONTEXT_SETUP)?;
        set_root_ctx(root_ctx);

        let child = make_child(&root)?;
        queue_root_job_holding(&child, |_cx, _child| Ok(()));

        let weak = Rc::downgrade(&child);
        drop(child);
        assert!(
            weak.upgrade().is_none(),
            "a root job must not keep the child alive"
        );
        Ok(())
    }
}
