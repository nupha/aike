use std::{
    cell::Cell,
    ffi::c_void,
    thread::{self, JoinHandle},
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue, Object,
    Runtime, ScopedContext,
};

use crate::{USER_CLASS_DEFS, class_llama_engine::get_engine_arc};

struct ServerState {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl ServerState {
    /// Send shutdown signal (non-blocking, idempotent).
    fn request_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn drop_server_state(state: &mut ServerState) {
    state.request_shutdown();
    if let Some(jh) = state.join_handle.take() {
        let _ = jh.join();
    }
}

fn server_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSHttpServerHandle::class_id())?
        .other_mut()
        .unwrap();
    let opaque = payload.opaque();
    if !opaque.is_null() {
        let state = unsafe { &mut *opaque.cast::<ServerState>() };
        state.request_shutdown();
    }
    Ok(JSValue::UNDEFINED)
}

fn server_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSHttpServerHandle::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            let mut state = Box::from_raw(p.cast::<ServerState>());
            drop_server_state(&mut state);
        }
    }
    Ok(JSValue::UNDEFINED)
}

const CLASS_SERVER_PROTO: &[FieldDef] = &[
    FieldDef::CALL("shutdown", 0, server_shutdown, 0),
    FieldDef::CALL("dispose", 0, server_dispose, 0),
];

pub struct JSHttpServerHandle;

impl JSHttpServerHandle {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("HttpServerHandle", CLASS_SERVER_PROTO, None).on_dispose(
                |_, opaque: *mut c_void, _| {
                    if !opaque.is_null() {
                        let mut state = unsafe { Box::from_raw(opaque.cast::<ServerState>()) };
                        drop_server_state(&mut state);
                    }
                },
            ),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    fn new_instance(ctx: &mut ScopedContext, state: ServerState) -> Result<Object, i32> {
        let boxed = Box::new(state);
        let opaque = Box::into_raw(boxed).cast::<c_void>();
        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<ServerState>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_http_server_handle(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpServerHandle::register(rt)
}

/// Engine.httpServe() – start an OpenAI-compatible HTTP server for this Engine.
///
/// Options:
///   host: string           (default "0.0.0.0")
///   port: number           (default 8080)
pub(super) fn engine_httpServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine_arc(ctx, &this)?;

    // Get model from engine's "model" property (set by createEngine)
    let model_val = this.get_property_str(ctx, "model")?;
    let model = crate::class_llama_model::get_llama_model(ctx, &model_val)?;

    // Parse options object
    let opts = if cc.length() > 0 {
        let v = cc.arg(0);
        if v.is_undefined() || v.is_null() {
            None
        } else {
            Some(v.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("options must be object");
                -1
            })?)
        }
    } else {
        None
    };

    // Defaults
    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8080;

    if let Some(opts) = opts.as_ref() {
        let v = opts.get_property_str(ctx, "host")?;
        if !v.is_undefined() {
            let s = v.to_string(ctx)?;
            host = s.content().encode_utf8(false).to_string();
        }

        let v = opts.get_property_str(ctx, "port")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 || n > 65535 {
                ctx.throw_type_error("port must be 1..65535");
                return Err(-1);
            }
            port = n as u16;
        }
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let join_handle = thread::spawn(move || {
        let async_rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("httpServe: failed to create tokio runtime: {e}");
                return;
            }
        };

        // start server
        if let Err(e) =
            async_rt.block_on(engine.http_server_with_shutdown(model, &host, port, shutdown_rx))
        {
            eprintln!("httpServe: server error: {e}");
        }
    });

    let state = ServerState {
        shutdown_tx: Some(shutdown_tx),
        join_handle: Some(join_handle),
    };

    let mut svr_handle_obj = JSHttpServerHandle::new_instance(ctx, state)?;

    // Keep engine alive while server handle exists
    svr_handle_obj.define_property_value_str(ctx, "engine", this, JS_PROP_ENUMERABLE)?;

    Ok(svr_handle_obj.into())
}
