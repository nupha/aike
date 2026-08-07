// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableStreamDefaultReader: the reader handle returned by
//! `ReadableStream.getReader()`.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::stream::readable_stream::{JSReadableStream, readable_stream_cancel},
};

use super::{QueueEntry, RsState, make_iterator_result};

pub(crate) struct ReadableStreamDefaultReaderPayload {
    pub(super) stream: Option<JSReadableStream>,
    pub(crate) closed_promise: Option<Promise>,
    /// True when acquired via `getReader({mode: 'byob'})`. A BYOB read writes
    /// the chunk into the caller-supplied view instead of allocating one.
    pub(super) byob: bool,
}
impl ReadableStreamDefaultReaderPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref cp) = self.closed_promise {
            cp.trace(gcx);
        }
    }

    /// Perform a `read()` on the underlying stream and return a Promise that
    /// resolves to `{ value, done }`. Internal callers (drain/tee/pipe/async
    /// iteration) call this directly instead of going through the JS `read`
    /// property; the JS API entry point `js_reader_read` delegates here.
    pub(crate) fn read(
        &self,
        ctx: &mut ScopedContext,
        byob_view: Option<TypedArray>,
    ) -> Result<Promise, i32> {
        let mut stream_val = self
            .stream
            .ok_or_else(|| ctx.throw_type_error("reader released"))?;

        // Mark disturbed before any early return: even a read on a closed or
        // errored stream counts as consuming it.
        if let Some(sp) = stream_val.payload_mut() {
            sp.disturbed = true;
        }

        // Copy to decouple borrow: stream_p borrows sv, leaving stream_val free
        // for mutable access later.
        let sv = stream_val;
        let stream_p = sv
            .payload()
            .ok_or_else(|| ctx.throw_internal_error("invalid stream"))?;

        match stream_p.state {
            RsState::Errored => {
                return Promise::from_reject_value(ctx, stream_p.stored_error);
            }
            RsState::Closed => {
                let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
                return Promise::from_resolve_value(ctx, result);
            }
            RsState::Readable => {}
        }

        if let Some(controller_val) = stream_p.controller {
            let cp = controller_val
                .payload()
                .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

            if !cp.queue.borrow().is_empty() {
                let mut q = cp.queue.borrow_mut();
                let entry = q.remove(0);
                let total = cp.queue_total_size.get();
                cp.queue_total_size.set(total - entry.size);

                // BYOB: copy as much of the chunk as fits into the caller's view
                // and push the remainder back so the next read continues where
                // this one stopped. Done before the close transition so a
                // partially consumed chunk keeps the stream readable.
                let chunk = if let Some(view) = byob_view {
                    let filled = byob_fill_view(ctx, view, entry.chunk, &mut q)?;
                    if !q.is_empty() {
                        cp.queue_total_size.set(cp.queue_total_size.get() + 1.0);
                    }
                    filled
                } else {
                    entry.chunk
                };

                // Per spec: after draining last item, if close_requested,
                // transition stream to closed state.
                if q.is_empty()
                    && cp.close_requested.get()
                    && let Some(sp) = stream_val.payload_mut()
                {
                    sp.state = RsState::Closed;
                }

                let result = make_iterator_result(ctx, chunk, false)?;
                return Promise::from_resolve_value(ctx, result);
            }

            if cp.close_requested.get() {
                let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
                return Promise::from_resolve_value(ctx, result);
            }

            // No data in queue — register a pending read on this controller so a
            // later enqueue()/close()/error() can resolve it. Stored per-controller
            // (not globally) so concurrent/sequential streams don't steal chunks.
            let (promise, rr) = Promise::create(ctx)?;
            cp.pending_read.borrow_mut().push([rr[0], rr[1]]);
            cp.call_controller_pull(ctx)?;
            return Ok(promise.into());
        }

        // No controller (e.g. detached) — nothing to read.
        let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
        Promise::from_resolve_value(ctx, result)
    }
}

const READER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("read", 0, js_reader_read, 0),
    FieldDef::FUNC("cancel", 1, js_reader_cancel, 0),
    FieldDef::FUNC("releaseLock", 0, js_reader_release_lock, 0),
    FieldDef::GETSET("closed", Some(js_reader_closed), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableStreamDefaultReader",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_reader_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error(
        "ReadableStreamDefaultReader must be obtained via ReadableStream.getReader()",
    ))
}

def_native_object!(
    "ReadableStreamDefaultReader",
    JSReadableStreamDefaultReader,
    ReadableStreamDefaultReaderPayload,
    READER_PROTO,
    Some(ClassCtorDef::new(js_reader_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableStreamDefaultReaderPayload::gc_trace
);

/// Copy `chunk` into the BYOB destination `view`, returning the view to hand
/// back to the caller. Bytes that do not fit are pushed to the front of `queue`
/// so the following read resumes from there.
///
/// The returned view spans exactly the written bytes, sharing the caller's
/// buffer, which is what makes `value.buffer` usable for the next read.
fn byob_fill_view(
    ctx: &mut ScopedContext,
    mut view: TypedArray,
    chunk: JSValue,
    queue: &mut Vec<QueueEntry>,
) -> Result<JSValue, i32> {
    let src: Vec<u8> = match chunk.as_object().and_then(|o| TypedArray::try_from(o).ok()) {
        Some(ta) => ta.with_bytes(|b| b.to_vec()),
        // Non-byte chunk in a byte stream: hand it through untouched.
        None => return Ok(chunk),
    };

    let n = view.with_bytes_mut(|dest| {
        let n = src.len().min(dest.len());
        dest[..n].copy_from_slice(&src[..n]);
        n
    });

    if n < src.len() {
        let rest = TypedArray::create_from_vec_u8(ctx, src[n..].to_vec())?;
        queue.insert(
            0,
            QueueEntry {
                chunk: JSValue::from(rest),
                size: 1.0,
            },
        );
    }

    // Return a view over the same buffer covering only the filled range, so the
    // caller can chain `value.buffer` plus `value.byteLength` as an offset.
    // `subarray` keeps the underlying buffer shared, unlike a fresh array.
    let view_val = JSValue::from(view);
    view_val.get_property_str(ctx, "subarray")?.call(
        ctx,
        view_val,
        &[JSValue::from(0.0), (n as f64).into()],
    )
}

fn js_reader_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;

    // A BYOB read takes the destination view as its only argument; the chunk is
    // written into that view's own buffer region rather than a fresh array.
    let byob_view = if p.byob {
        let view = cc.arg(0);
        let obj = view
            .as_object()
            .ok_or_else(|| ctx.throw_type_error("view must be a TypedArray"))?;
        let ta = TypedArray::try_from(obj)
            .map_err(|_| ctx.throw_type_error("view must be a TypedArray"))?;
        if ta.length() == 0 {
            return Err(ctx.throw_type_error("view must have non-zero length"));
        }
        Some(ta)
    } else {
        None
    };

    p.read(ctx, byob_view).map(JSValue::from)
}

fn js_reader_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let reason = cc.arg(0);
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;
    match &p.stream {
        Some(stream_val) => readable_stream_cancel(ctx, &JSValue::from(*stream_val), reason),
        None => Err(ctx.throw_type_error("reader released")),
    }
}

fn js_reader_release_lock(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    if let Some(p) = JSReadableStreamDefaultReader::payload_mut_of(&mut this)
        && let Some(mut stream) = p.stream.take()
        && let Some(sp) = stream.payload_mut()
    {
        sp.reader = None;
    }
    Ok(JSValue::UNDEFINED)
}

fn js_reader_closed(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream reader"))?;
    if let Some(cp) = p.closed_promise {
        return Ok(JSValue::from(cp));
    }
    Ok(JSValue::UNDEFINED)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream_default_reader(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStreamDefaultReader::register(rt)
}
