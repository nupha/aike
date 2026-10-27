// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! JS-side half of a streaming client call: the async-iterator fetcher.
//!
//! The reading task (in `class_client.rs` / `class_client_response.rs`)
//! commits results into a slot from the tokio side; this type polls that slot
//! on the JS thread, applies the user's response normalizer, and forwards an
//! early `return()`/`break` to the cancellation flag.

use std::sync::Arc;

use zuqe::{
    JSValue, ScopedContext,
    builtins::{
        iterator::{AsyncIteratorFetcher, IterFetchStatus},
        promise::PromiseResultSlot,
    },
    gc_lite::{GcTrace, GcTraceCtx},
};

use super::client_call::StreamCancel;

/// Stream-consumer half of a `LlmClient.chat()`/`responses()` iterator.
///
/// Polls the result slot on the JS thread (`try_fetch`), applies the response
/// normalizer to yielded chunks, and forwards early `return()`/`break` to
/// [`StreamCancel`]. Not `Send`: it holds the `normalizer` JS value and lives
/// behind the engine's `Rc`.
pub(super) struct LlmStreamFetcher<D: Send> {
    slot: PromiseResultSlot<D>,
    cancel: Arc<StreamCancel>,
    normalizer: JSValue,
    convert: Box<dyn Fn(&mut ScopedContext, D) -> Result<Option<JSValue>, i32>>,
}

impl<D: Send + 'static> LlmStreamFetcher<D> {
    pub(super) fn new(
        slot: PromiseResultSlot<D>,
        cancel: Arc<StreamCancel>,
        normalizer: JSValue,
        convert: impl Fn(&mut ScopedContext, D) -> Result<Option<JSValue>, i32> + 'static,
    ) -> Self {
        Self {
            slot,
            cancel,
            normalizer,
            convert: Box::new(convert),
        }
    }
}

impl<D: Send + 'static> GcTrace for LlmStreamFetcher<D> {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        self.normalizer.trace(gcx);
    }
}

impl<D: Send + 'static> AsyncIteratorFetcher for LlmStreamFetcher<D> {
    fn try_fetch(&self, cx: &mut ScopedContext) -> Result<IterFetchStatus, i32> {
        let Some(data) = self.slot.try_take() else {
            return Ok(IterFetchStatus::Wait);
        };
        match (self.convert)(cx, data) {
            Ok(Some(v)) => {
                let v = if self.normalizer.is_callable() {
                    self.normalizer.call(cx, JSValue::UNDEFINED, &[v])?
                } else {
                    v
                };
                Ok(IterFetchStatus::Value(v))
            }
            Ok(None) => Ok(IterFetchStatus::Done),
            Err(e) => Err(e),
        }
    }

    fn close(&self) {
        self.cancel.cancel();
    }
}
