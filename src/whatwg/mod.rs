// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG standard API implementations for Aike runtime.

pub mod abort;
pub mod console;
pub mod fetch;
pub(crate) mod http_cache;
pub mod stream;
pub mod text_decoder;
pub mod text_encoder;
pub mod timeout;
pub mod url;

use zuqe::{NativeObject, Object, ScopedContext};

/// Install all WHATWG-standard global APIs.
pub(crate) fn install_whatwg(ctx: &mut ScopedContext, mut global: Object) -> Result<(), i32> {
    ctx.use_classes(&[
        abort::JSAbortController::class_id(),
        abort::JSAbortSignal::class_id(),
        text_decoder::JSTextDecoder::class_id(),
        text_encoder::JSTextEncoder::class_id(),
        url::JSURL::class_id(),
        url::JSURLSearchParams::class_id(),
        stream::JSReadableStream::class_id(),
        stream::JSReadableStreamDefaultReader::class_id(),
        stream::JSReadableStreamDefaultController::class_id(),
        stream::JSWritableStream::class_id(),
        stream::JSWritableStreamDefaultWriter::class_id(),
        stream::JSWritableStreamDefaultController::class_id(),
        stream::JSTransformStream::class_id(),
        stream::JSTransformStreamDefaultController::class_id(),
        stream::JSByteLengthQueuingStrategy::class_id(),
        stream::JSCountQueuingStrategy::class_id(),
    ])?;

    console::install_console(ctx, &mut global)?;
    timeout::install_timeout(ctx, global)?;
    abort::install_abort_statics(ctx, &mut global)?;
    fetch::install_fetch(ctx, global)?;

    Ok(())
}
