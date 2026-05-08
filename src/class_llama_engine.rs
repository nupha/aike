use std::{
    cell::Cell,
    ffi::c_void,
    sync::{Arc, Mutex},
};

use linkme::distributed_slice;
use llama_cpp_ffi::ToolChoice as LarkToolChoice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
};

use crate::{
    USER_CLASS_DEFS,
    chat_response::{ChatResponseParser, ChatStreamItem},
    class_llama_client::{ParsedChatOptions, chunk_to_js, parse_chat_options, parse_messages},
    engine::{Engine, GrammarConfig, OutputMode, TaskConfig},
    helpers::{build_chat_prompt_tokens, compile_tools_grammar, now_unix_s, to_lark_tool_choice},
};

pub(crate) const ERR_CANCELLED: &str = "cancelled";
pub(crate) const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";

pub(super) struct EngineParams {
    pub idle_sleep_ms: u64,
    pub slot_match_threshold: f32,
    pub max_tokens: usize,
}

pub(super) fn parse_engine_params(
    ctx: &mut ScopedContext,
    opts: Option<&Object>,
) -> Result<EngineParams, i32> {
    let mut idle_sleep_ms = 1u64;
    let mut slot_match_threshold = 0.5f32;
    let mut max_tokens = 0usize;

    if let Some(opts) = opts {
        // idleSleepMs
        let v = opts.get_property_str(ctx, "idleSleepMs")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                idle_sleep_ms = n as u64;
            } else {
                ctx.throw_type_error("idleSleepMs must be >= 0");
                return Err(-1);
            }
        }

        // slotMatchThreshold
        let v = opts.get_property_str(ctx, "slotMatchThreshold")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..=1.0).contains(&d) {
                slot_match_threshold = d as f32;
            } else {
                ctx.throw_type_error("slotMatchThreshold must be in [0, 1]");
                return Err(-1);
            }
        }

        // maxCompletionTokens
        let v = opts.get_property_str(ctx, "maxCompletionTokens")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n < 0 {
                ctx.throw_type_error("maxCompletionTokens must be >= 0");
                return Err(-1);
            }
            max_tokens = n as usize;
        }
    }

    Ok(EngineParams {
        idle_sleep_ms,
        slot_match_threshold,
        max_tokens,
    })
}

fn get_engine<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<&'a Mutex<Engine>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaEngine::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("Engine was disposed");
        Err(-1)
    } else {
        let k = unsafe { &*opaque.cast::<Mutex<Engine>>() };
        Ok(k)
    }
}

pub(super) fn get_engine_arc(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Arc<Mutex<Engine>>, i32> {
    // opaque was created from Arc::into_raw(), so it points to the Engine
    // inside the Arc allocation. Increment the strong count so the returned
    // Arc can outlive the JS object.
    let engine_ref = get_engine(ctx, val)?;
    let ptr = engine_ref as *const Mutex<Engine>;
    unsafe { Arc::increment_strong_count(ptr) };
    Ok(unsafe { Arc::from_raw(ptr) })
}

pub(super) const MAGIC_CHAT_ONCE: i32 = 2;
pub(super) const MAGIC_CHAT_STREAM: i32 = 3;

fn js_engine_chat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    // Parse messages (required)
    let messages_val = cc.arg(0);
    let messages = parse_messages(ctx, &messages_val)?;

    let mut id_slot: Option<usize> = None;
    let mut cache_prompt = false;
    let mut include_usage = false;

    let mut params = ParsedChatOptions {
        model: String::new(),
        temperature: None,
        max_completion_tokens: None,
        top_p: None,
        n: None,
        stop: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        response_format: None,
        stream_options: None,
        seed: None,
        reasoning_effort: None,
    };

    // Parse options (optional)
    if cc.length() > 1 {
        let val = cc.arg(1);
        if !val.is_undefined() && !val.is_null() {
            let opts_obj = val.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("options must be object");
                -1
            })?;

            params = parse_chat_options(ctx, &opts_obj)?;

            let v = opts_obj.get_property_str(ctx, "idSlot")?;
            if !v.is_undefined() {
                let n = v.to_number(ctx)?.to_i32();
                if n >= 0 {
                    id_slot = Some(n as usize);
                }
            }

            let v = opts_obj.get_property_str(ctx, "cachePrompt")?;
            if !v.is_undefined() {
                cache_prompt = v.to_bool()?;
            }

            let v = opts_obj.get_property_str(ctx, "includeUsage")?;
            if !v.is_undefined() {
                include_usage = v.to_bool()?;
            }
        }
    }

    let prompt_tokens = build_chat_prompt_tokens(
        engine.lock().unwrap().model(),
        &messages,
        params.tools.as_deref(),
        params.tool_choice.as_ref(),
        params.parallel_tool_calls,
        params.reasoning_effort.as_ref(),
        params.response_format.as_ref(),
    )
    .map_err(|e| {
        ctx.throw_internal_error(&format!("build prompt tokens failed: {e}"));
        -1
    })?;

    // Build prompt tokens
    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;

    // Build task config
    let mut task_config = TaskConfig::new()
        .temperature(params.temperature.unwrap_or(0.75))
        .top_p(params.top_p.unwrap_or(1.0))
        .max_tokens(
            params
                .max_completion_tokens
                .map_or(engine.lock().unwrap().default_max_tokens(), |n| n as usize),
        )
        .output_mode(if is_stream {
            OutputMode::Continuous
        } else {
            OutputMode::OneShot
        });

    if let Some(stop_words) = params.stop.as_ref().filter(|w| !w.is_empty()) {
        task_config = task_config.stop_words(stop_words.clone());
    }

    if let Some(tools) = params.tools.as_ref().filter(|t| !t.is_empty()) {
        let tc = params
            .tool_choice
            .as_ref()
            .map(to_lark_tool_choice)
            .unwrap_or(LarkToolChoice::Auto);

        if tc != LarkToolChoice::None {
            task_config = task_config.grammar(GrammarConfig {
                lark_grammar: compile_tools_grammar(tools, tc),
                tool_choice: tc,
            });
        }
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let model_name = engine.lock().unwrap().model_name().to_string();

    // Push task to engine
    let rx = engine
        .lock()
        .unwrap()
        .push_task_with_params(prompt_tokens, task_config, id_slot, cache_prompt)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    let mut parser = ChatResponseParser::new(
        params.reasoning_effort.is_some(),
        n_prompt_tokens,
        include_usage,
    );
    parser.set_respond_id(respond_id.clone());
    parser.set_model_name(model_name.clone());

    if is_stream {
        // Streaming: AsyncIterableIterator
        let (stream_tx, stream_rx) = crossbeam::channel::bounded::<ChatStreamItem>(8);

        std::thread::spawn(move || {
            parser.parse_stream(rx, std::time::Duration::from_secs(u64::MAX), |item| {
                stream_tx.send(item).is_ok()
            });
        });

        AsyncIterableIterator::create_with_receiver::<ChatStreamItem>(
            ctx,
            stream_rx,
            move |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    ChatStreamItem::Done => Ok(None),
                    ChatStreamItem::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                    _ => {
                        let chunk = msg.into_chunk(&respond_id, &model_name, now_unix_s() as u32);
                        chunk_to_js(cx, &chunk).map(Some)
                    }
                }
            },
        )
        .map(JSValue::from)
    } else {
        // Non-streaming: Promise
        let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
        let promise =
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, res| match res {
                Ok(val) => JSValue::from_serde_json(cx, &val),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            })?;

        std::thread::spawn(move || {
            let result = parser.parse_all(rx, std::time::Duration::from_secs(u64::MAX));
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}

fn js_engine_httpServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine_arc(ctx, &this)?;

    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8080;

    // Parse options
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

    if engine.lock().unwrap().is_serving() {
        ctx.throw_type_error("engine is already serving");
        return Err(-1);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    engine.lock().unwrap().set_serving(shutdown_tx);

    // Start HTTP server
    let result_slot = PromiseResultSlot::<Result<(), String>>::new();

    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, res| match res {
            Ok(()) => Ok(JSValue::UNDEFINED),
            Err(e) => {
                cx.throw_internal_error(&e);
                Err(-1)
            }
        })?;

    tokio::spawn(async move {
        let res = crate::serve::http_serve(engine.clone(), &host, port, shutdown_rx).await;

        engine.lock().unwrap().unset_serving();
        let _ = result_slot.commit(res);
    });

    Ok(promise.into())
}

fn js_engine_stopServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    engine.lock().unwrap().request_stop();
    Ok(JSValue::UNDEFINED)
}

fn js_engine_isServing(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    Ok(JSValue::Bool(engine.lock().unwrap().is_serving()))
}

fn js_engine_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaEngine::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Arc::from_raw(p.cast::<Engine>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

const CLASS_ENGINE_PROTO: &[FieldDef] = &[
    FieldDef::CALL("chat", 2, js_engine_chat, MAGIC_CHAT_STREAM as i16),
    FieldDef::CALL("chatOnce", 2, js_engine_chat, MAGIC_CHAT_ONCE as i16),
    FieldDef::CALL("httpServe", 1, js_engine_httpServe, 0),
    FieldDef::CALL("stopServe", 0, js_engine_stopServe, 0),
    FieldDef::GETSET("isServing", Some(js_engine_isServing), None, 0),
    FieldDef::CALL("dispose", 0, js_engine_dispose, 0),
];

pub struct JSLlamaEngine;

impl JSLlamaEngine {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("Engine", CLASS_ENGINE_PROTO, None).on_dispose(
                |_, opaque: *mut c_void, _| {
                    if !opaque.is_null() {
                        drop(unsafe { Arc::from_raw(opaque.cast::<Engine>()) });
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

    pub(super) fn new_instance(
        ctx: &mut ScopedContext,
        engine: Arc<Mutex<Engine>>,
    ) -> Result<Object, i32> {
        let opaque = Arc::into_raw(engine) as *mut c_void;

        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Arc::from_raw(p.opaque().cast::<Mutex<Engine>>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_engine(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaEngine::register(rt)
}
