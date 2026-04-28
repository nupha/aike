use std::sync::{Arc, Mutex};

use zuqe::{CallContext, JSResult, JSValue, ScopedContext, builtins::promise::Promise};

use crate::{class_llama_engine::get_engine_arc, class_llama_model::get_llama_model};

/// Engine.httpServe() – start an OpenAI-compatible HTTP server for this Engine.
///
/// Returns a Promise that resolves when the server shuts down.
/// The server runs on the existing tokio runtime (no extra thread).
///
/// Options:
///   host: string           (default "0.0.0.0")
///   port: number           (default 8080)
pub(super) fn engine_httpServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine_arc(ctx, &this)?;

    // Get model from engine's "model" property (set by createEngine)
    let model_val = this.get_property_str(ctx, "model")?;
    let model = get_llama_model(ctx, &model_val)?;

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

    if engine.is_serving() {
        ctx.throw_type_error("engine is already serving");
        return Err(-1);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    engine.set_shutdown_tx(Some(shutdown_tx));
    engine.set_serving(true);

    let result_slot = Arc::new(Mutex::new(None::<Result<(), String>>));

    let promise = Promise::create_with_job(ctx, {
        let res = result_slot.clone();
        move |cx| match res.lock().unwrap().take() {
            Some(Ok(())) => Ok(Some(JSValue::UNDEFINED)),
            Some(Err(e)) => {
                cx.throw_internal_error(&e);
                Err(-1)
            }
            None => Ok(None),
        }
    })?;

    tokio::spawn(async move {
        let engine_for_server = engine.clone();
        let res = engine_for_server
            .http_server_with_shutdown(model, &host, port, shutdown_rx)
            .await;
        engine.set_serving(false);
        engine.set_shutdown_tx(None);
        *result_slot.lock().unwrap() = Some(res);
    });

    Ok(promise.into())
}
