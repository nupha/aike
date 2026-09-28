// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_client;
mod class_client_response;

pub(crate) use class_client::JSLlmClient;

// Standard OpenAI Chat Completion response field names
pub(crate) const OUTMSG_FIELD_REASONING: &str = "reasoning_content";
pub(crate) const OUTMSG_FIELD_CHOICES: &str = "choices";

// Magic values identifying each ai module method entry point.
pub(crate) const MAGIC_CHAT_ONCE: i32 = 2;
pub(crate) const MAGIC_CHAT_STREAM: i32 = 3;
pub(crate) const MAGIC_RESPONSES_STREAM: i32 = 4;
pub(crate) const MAGIC_RESPONSES_ONCE: i32 = 5;

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, ScopedContext};

const MOD_AI_ENTRIES: &[FieldDef] = &[FieldDef::CLASS(
    JSLlmClient::CLASS_NAME,
    JSLlmClient::class_id,
    JS_PROP_ENUMERABLE,
)];

pub fn init_mod_ai(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("ai", MOD_AI_ENTRIES)?;
    Ok(())
}
