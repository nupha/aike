// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use zuqe::builtins::typed_array::TypedArray;
use zuqe::{CallContext, JSResult, JSValue, ScopedContext};

/// `Aike.dumpValue(val)` — serialize `val` into a self-contained bytecode blob
/// and return it as a `Uint8Array`. The blob can be reloaded in any Runtime via
/// `Aike.restoreValue` (cross-runtime safe: primitives, objects/arrays,
/// functions, generators, modules). Throws when the value cannot be persisted.
pub(super) fn js_dump_value(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let blob = JSValue::dump(ctx, cc.arg(0))
        .map_err(|e| ctx.throw_type_error(&format!("dumpValue: {e}")))?;
    let ta = TypedArray::create_from_vec_u8(ctx, blob.to_vec())
        .map_err(|_| ctx.throw_type_error("dumpValue: cannot allocate Uint8Array"))?;
    Ok(ta.into())
}

/// `Aike.restoreValue(bytes)` — rebuild the value previously produced by
/// `dumpValue` from a `Uint8Array` and return it. Throws when `bytes` is not a
/// `Uint8Array`, the blob is malformed/truncated, or a referenced value cannot
/// be reconstructed (e.g. a native function whose name is not exposed on this
/// runtime's global object).
pub(super) fn js_restore_value(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let ta = TypedArray::try_from(cc.arg(0))
        .map_err(|_| ctx.throw_type_error("restoreValue: argument must be a Uint8Array"))?;
    let buf = ta.bytes().to_vec();
    JSValue::restore(ctx, &buf).map_err(|e| ctx.throw_type_error(&format!("restoreValue: {e}")))
}
