// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! `Aike.gzip` / `Aike.gunzip` — byte-level gzip compression helpers.
//!
//! Not a WHATWG API: the Compression Standard defines the streaming
//! `CompressionStream`/`DecompressionStream` classes. These two aike
//! conveniences (memo §5.1, same family as `bytesToBase64`/`bytesToHex`)
//! compress a whole buffer in the RFC 1952 gzip format in one call.

use std::io::{Read, Write};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use zuqe::{CallContext, JSResult, JSValue, ScopedContext, builtins::typed_array::TypedArray};

use crate::whatwg::crypto::get_buffer_source_bytes;

/// `Aike.gzip(data)` — gzip-compress a `BufferSource`, returning a `Uint8Array`.
/// Compression is a complete RFC 1952 member (magic `1f 8b`, deflate, CRC-32).
pub(crate) fn js_gzip(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let bytes = get_buffer_source_bytes(ctx, &cc.arg(0))?;

    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    // Writing to a Vec is infallible; keep the io::Result handling explicit.
    enc.write_all(&bytes)
        .map_err(|e| ctx.throw_internal_error(&e.to_string()))?;
    let out = enc
        .finish()
        .map_err(|e| ctx.throw_internal_error(&e.to_string()))?;

    TypedArray::create_from_vec_u8(ctx, out).map(JSValue::from)
}

/// `Aike.gunzip(data)` — gzip-decompress a `BufferSource`, returning a
/// `Uint8Array`. Throws a RangeError if the input is not a valid gzip stream.
pub(crate) fn js_gunzip(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let bytes = get_buffer_source_bytes(ctx, &cc.arg(0))?;

    let mut dec = GzDecoder::new(bytes.as_slice());
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|_| ctx.throw_range_error("`data` must be valid gzip data"))?;

    TypedArray::create_from_vec_u8(ctx, out).map(JSValue::from)
}
