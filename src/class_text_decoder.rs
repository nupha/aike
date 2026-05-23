// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassDef, ClassID, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext,
};

use crate::USER_CLASS_DEFS;

/// Supported encodings for TextDecoder
#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Iso8859_1,
}

impl Encoding {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "utf-8" | "utf8" => Some(Encoding::Utf8),
            "utf-16le" | "utf16le" => Some(Encoding::Utf16Le),
            "utf-16be" | "utf16be" => Some(Encoding::Utf16Be),
            "iso-8859-1" | "iso8859-1" | "latin1" => Some(Encoding::Iso8859_1),
            _ => None,
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<String, String> {
        match self {
            Encoding::Utf8 => {
                String::from_utf8(bytes.to_vec()).map_err(|e| format!("Invalid UTF-8: {}", e))
            }
            Encoding::Utf16Le => decode_utf16_le(bytes),
            Encoding::Utf16Be => decode_utf16_be(bytes),
            Encoding::Iso8859_1 => Ok(bytes.iter().map(|&b| b as char).collect()),
        }
    }
}

fn decode_utf16_le(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("Invalid UTF-16LE: odd number of bytes".to_string());
    }

    let mut result = String::new();
    for chunk in bytes.chunks(2) {
        let code_point = u16::from_le_bytes([chunk[0], chunk[1]]) as u32;
        match char::from_u32(code_point) {
            Some(c) => result.push(c),
            None => return Err(format!("Invalid UTF-16LE code point: U+{:04X}", code_point)),
        }
    }
    Ok(result)
}

fn decode_utf16_be(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("Invalid UTF-16BE: odd number of bytes".to_string());
    }

    let mut result = String::new();
    for chunk in bytes.chunks(2) {
        let code_point = u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
        match char::from_u32(code_point) {
            Some(c) => result.push(c),
            None => return Err(format!("Invalid UTF-16BE code point: U+{:04X}", code_point)),
        }
    }
    Ok(result)
}

fn js_text_decoder_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    // Parse encoding
    let encoding = if cc.length() > 0 {
        let enc_str = cc.arg(0).to_string(ctx)?;
        let enc_name = enc_str.content().encode_utf8(false).to_string();
        Encoding::from_str(&enc_name).ok_or_else(|| {
            ctx.throw_range_error(&format!("Unsupported encoding: {}", enc_name));
            -1
        })?
    } else {
        Encoding::Utf8 // Default encoding
    };

    // Parse options
    let fatal = if cc.length() > 1 {
        let arg1 = cc.arg(1);
        match arg1.as_object() {
            Some(opts) => {
                let fatal_val = opts.get_property_str(ctx, "fatal")?;
                if !fatal_val.is_undefined() {
                    fatal_val.to_bool()?
                } else {
                    false
                }
            }
            None => {
                ctx.throw_type_error("Options must be an object");
                return Err(-1);
            }
        }
    } else {
        false
    };

    let ignore_bom = if cc.length() > 1 {
        let arg1 = cc.arg(1);
        match arg1.as_object() {
            Some(opts) => {
                let ignore_bom_val = opts.get_property_str(ctx, "ignoreBOM")?;
                if !ignore_bom_val.is_undefined() {
                    ignore_bom_val.to_bool()?
                } else {
                    false
                }
            }
            None => {
                ctx.throw_type_error("Options must be an object");
                return Err(-1);
            }
        }
    } else {
        false
    };

    // Store encoding and options in object properties
    let mut obj = ctx
        .create_class_instance(new_target, JSTextDecoder::class_id())
        .map_err(|_| {
            ctx.throw_internal_error("Failed to create TextDecoder instance");
            -1
        })?;

    // Store encoding as string property
    let enc_name = match encoding {
        Encoding::Utf8 => "utf-8",
        Encoding::Utf16Le => "utf-16le",
        Encoding::Utf16Be => "utf-16be",
        Encoding::Iso8859_1 => "iso-8859-1",
    };
    let enc_str = ctx.create_string_from_utf8(enc_name.as_bytes().to_vec())?;
    obj.define_property_value_str(ctx, "__encoding", enc_str.into(), 0)?;

    // Store fatal flag
    obj.define_property_value_str(ctx, "__fatal", JSValue::Bool(fatal), 0)?;

    // Store ignoreBOM flag
    obj.define_property_value_str(ctx, "__ignoreBom", JSValue::Bool(ignore_bom), 0)?;

    Ok(obj.into())
}

fn js_text_decoder_decode(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    // Get stored properties
    let enc_val = this.get_property_str(ctx, "__encoding")?;
    let enc_name = enc_val.to_string(ctx)?;
    let encoding =
        Encoding::from_str(enc_name.content().encode_utf8(false).as_ref()).ok_or_else(|| {
            ctx.throw_internal_error("Invalid encoding stored");
            -1
        })?;

    let fatal_val = this.get_property_str(ctx, "__fatal")?;
    let fatal = fatal_val.to_bool()?;

    // Get input buffer
    match cc.arg(0) {
        s @ JSValue::String(_) => return Ok(s),
        JSValue::Object(o) => {
            let p = o.payload();

            let bytes = if let Some(ab) = p.array_buffer() {
                ab.bytes()
            } else if let Some(ta) = p.typed_array() {
                ta.bytes()
            } else {
                ctx.throw_type_error("Input must be an ArrayBuffer or TypedArray");
                return Err(-1);
            };

            // Handle BOM if not ignored
            let ignore_bom_val = this.get_property_str(ctx, "__ignoreBom")?;
            let ignore_bom = ignore_bom_val.to_bool()?;
            let bytes_to_decode = if bytes.len() >= 2 && !ignore_bom {
                // Check for BOM
                if bytes[0] == 0xEF && bytes[1] == 0xBB {
                    // UTF-8 BOM
                    if bytes.len() >= 3 && bytes[2] == 0xBF {
                        &bytes[3..]
                    } else {
                        &bytes[..]
                    }
                } else if bytes[0] == 0xFE && bytes[1] == 0xFF {
                    // UTF-16 BE BOM
                    &bytes[2..]
                } else if bytes[0] == 0xFF && bytes[1] == 0xFE {
                    // UTF-16 LE BOM
                    &bytes[2..]
                } else {
                    &bytes[..]
                }
            } else {
                &bytes[..]
            };

            // Decode
            match encoding.decode(bytes_to_decode) {
                Ok(s) => ctx
                    .create_string_from_utf8(s.into_bytes())
                    .map(JSValue::String),
                Err(e) => {
                    if fatal {
                        ctx.throw_type_error(&e);
                        Err(-1)
                    } else {
                        // Replace invalid sequences with U+FFFD
                        Ok(ctx
                            .create_string_from_utf8("".as_bytes().to_vec())
                            .map(JSValue::String)
                            .unwrap_or_else(|_| {
                                JSValue::String(ctx.create_string_from_str("").unwrap())
                            }))
                    }
                }
            }
        }
        _ => {
            ctx.throw_type_error("Input must be TypedArray, ArrayBuffer or Object");
            return Err(-1);
        }
    }
}

fn js_text_decoder_encoding_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let enc_val = this.get_property_str(ctx, "__encoding")?;
    Ok(enc_val)
}

fn js_text_decoder_fatal_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let fatal_val = this.get_property_str(ctx, "__fatal")?;
    Ok(fatal_val)
}

fn js_text_decoder_ignore_bom_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let ignore_bom_val = this.get_property_str(ctx, "__ignoreBom")?;
    Ok(ignore_bom_val)
}

const CLASS_TEXT_DECODER_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("encoding", Some(js_text_decoder_encoding_getter), None, 0),
    FieldDef::GETSET("fatal", Some(js_text_decoder_fatal_getter), None, 0),
    FieldDef::GETSET(
        "ignoreBOM",
        Some(js_text_decoder_ignore_bom_getter),
        None,
        0,
    ),
    FieldDef::FUNC("decode", 1, js_text_decoder_decode, 0),
];

pub struct JSTextDecoder;

impl JSTextDecoder {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(ClassDef::new(
            "TextDecoder",
            CLASS_TEXT_DECODER_PROTO,
            Some(ClassCtorDef::new(js_text_decoder_constructor, 2, true).expose_to_global()),
        ))?;

        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_text_decoder(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTextDecoder::register(rt)
}
