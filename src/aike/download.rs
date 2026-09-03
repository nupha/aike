// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `Aike.download(url, destPath?, options?)` — download a URL to a file or bytes.
//!
//! Thin sugar over reqwest + the fs atomic-write machinery (memo §5.4). The HTTP
//! client is built the same way as `fetch` (honors `AIKE_TLS_INSECURE_HOSTS`,
//! env proxy, connect timeout, redirects). With `destPath` the body is streamed
//! to a temp file and atomically renamed; without it the bytes are returned. An
//! `AbortSignal` (`options.signal`) cancels an in-flight download — the partial
//! temp file is removed and the promise rejects with the signal's reason.

use std::path::PathBuf;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use url::Url;
use zuqe::{
    CallContext, ErrorKind, Exception, JSResult, JSValue, NativeObject, ScopedContext,
    builtins::{
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
};

use crate::{
    fs::TempFileGuard,
    http::client::config::{HttpClientConfig, tls_insecure_for_host},
    whatwg::{
        abort::{JSAbortSignal, signal_is_aborted, signal_notify, signal_reason},
        dom::make_dom_exception,
    },
};

enum DownloadOutcome {
    Bytes(Vec<u8>),
    Path(String),
}

enum DownloadError {
    Abort,
    Other(String),
}

/// `Aike.download(url, destPath?, options?)` → `Promise<string | Uint8Array>`.
pub(super) fn js_download(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // url — required string
    let url = cc
        .arg(0)
        .as_string(false)
        .ok_or_else(|| ctx.throw_type_error("`url` must be a string"))?
        .content()
        .encode_utf8(false)
        .to_string();

    // destPath — optional string
    let mut dest_path: Option<PathBuf> = None;
    if cc.length() > 1 && !cc.arg(1).is_undefined() {
        let Some(s) = cc.arg(1).as_string(false) else {
            return Err(ctx.throw_type_error("`destPath` must be a string"));
        };
        dest_path = Some(PathBuf::from(s.content().encode_utf8(false).to_string()));
    }

    // options — optional object; currently only `signal`
    let mut signal: Option<JSValue> = None;
    if cc.length() > 2 {
        let Some(opts) = cc.arg(2).as_object() else {
            return Err(ctx.throw_type_error("download options must be an object"));
        };
        let sig = opts.get_property_str(ctx, "signal")?;
        if !sig.is_undefined() {
            if JSAbortSignal::from_value(sig).is_none() {
                return Err(ctx.throw_type_error("`signal` must be an AbortSignal"));
            }
            signal = Some(sig);
        }
    }

    // Validate the URL and pick the per-host TLS escape hatch (same as fetch).
    let insecure = match Url::parse(&url) {
        Ok(u) => u
            .host_str()
            .map(|h| tls_insecure_for_host(h, u.port_or_known_default()))
            .unwrap_or(false),
        Err(_) => {
            let reason = ctx
                .create_error_object(ErrorKind::Type, "`url` must be a valid URL", false)
                .map(JSValue::from)
                .unwrap_or(JSValue::UNDEFINED);
            return Promise::from_reject_value(ctx, reason).map(JSValue::from);
        }
    };

    let cfg = HttpClientConfig::default();
    let client = match cfg.build_http_client_ex(insecure) {
        Ok(c) => c,
        Err(e) => {
            return Err(ctx.throw_internal_error(&format!("failed to build HTTP client: {e}")));
        }
    };

    let notify = signal.as_ref().and_then(signal_notify);
    let signal_for_reason = signal;

    // Already-aborted signal: reject on a microtask without any network I/O.
    if signal_for_reason.as_ref().is_some_and(signal_is_aborted) {
        let reason = signal_for_reason
            .as_ref()
            .and_then(signal_reason)
            .unwrap_or_else(|| {
                make_dom_exception(ctx, "AbortError", "The operation was aborted.")
                    .unwrap_or(JSValue::UNDEFINED)
            });
        return Promise::from_reject_value(ctx, reason).map(JSValue::from);
    }

    // Result channel; the JS-side closure maps the outcome to a JS value and
    // holds the signal so an *in-flight* abort rejects with its live reason
    // (a JSValue is not Send, so it must be read on the JS thread).
    let result_slot = PromiseResultSlot::<Result<DownloadOutcome, DownloadError>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), move |cx, data| -> JSResult {
            match data {
                Ok(DownloadOutcome::Bytes(b)) => {
                    TypedArray::create_from_vec_u8(cx, b).map(JSValue::from)
                }
                Ok(DownloadOutcome::Path(p)) => {
                    cx.create_string_from_str(&p).map(JSValue::make_string)
                }
                Err(DownloadError::Abort) => {
                    let reason = signal_for_reason
                        .as_ref()
                        .and_then(signal_reason)
                        .unwrap_or_else(|| {
                            make_dom_exception(cx, "AbortError", "The operation was aborted.")
                                .unwrap_or(JSValue::UNDEFINED)
                        });
                    cx.throw(Exception::Error(reason));
                    Err(-1)
                }
                Err(DownloadError::Other(msg)) => Err(cx.throw_internal_error(&msg)),
            }
        })?;

    // Run the download on tokio, raced against the signal's Notify. When abort
    // wins, the download future is dropped (cancelling the request and the file
    // write); its TempFileGuard removes the partial temp file.
    let rs = result_slot.clone();
    tokio::spawn(async move {
        let fut = download_impl(client, url, dest_path);
        let outcome = match notify {
            Some(n) => tokio::select! {
                biased;
                _ = n.notified() => Err(DownloadError::Abort),
                r = fut => r,
            },
            None => fut.await,
        };
        let _ = rs.commit(outcome);
    });

    Ok(promise.into())
}

async fn download_impl(
    client: reqwest::Client,
    url: String,
    dest: Option<PathBuf>,
) -> Result<DownloadOutcome, DownloadError> {
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| DownloadError::Other(format!("request failed: {e}")))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| DownloadError::Other(format!("request failed: {e}")))?;

    match dest {
        Some(dest) => {
            // Content goes to `<dest>.tmp`; `guard.commit()` atomically renames
            // it over the final path. On error/abort the guard's Drop removes
            // the partial temp file.
            let mut guard = TempFileGuard::new(dest.clone());
            let tmp = guard.temp_path().to_path_buf();
            let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| {
                DownloadError::Other(format!("cannot create {}: {e}", tmp.display()))
            })?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| DownloadError::Other(format!("read failed: {e}")))?;
                file.write_all(&chunk)
                    .await
                    .map_err(|e| DownloadError::Other(format!("write failed: {e}")))?;
            }
            drop(file);
            guard
                .commit()
                .await
                .map_err(|e| DownloadError::Other(format!("replace failed: {e}")))?;
            Ok(DownloadOutcome::Path(dest.to_string_lossy().into_owned()))
        }
        None => {
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| DownloadError::Other(format!("read failed: {e}")))?;
            Ok(DownloadOutcome::Bytes(bytes.to_vec()))
        }
    }
}
