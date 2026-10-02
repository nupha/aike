// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_client;
mod class_client_response;
mod common;
mod system1;

pub(crate) use class_client::JSLlmClient;
pub(crate) use system1::{
    JSSystem1Client, js_question_choice, js_question_noul, js_question_score,
};

// Standard OpenAI Chat Completion response field names
pub(crate) const OUTMSG_FIELD_REASONING: &str = "reasoning_content";
pub(crate) const OUTMSG_FIELD_CHOICES: &str = "choices";

// Magic values identifying each ai module method entry point.
pub(crate) const MAGIC_CHAT_ONCE: i32 = 2;
pub(crate) const MAGIC_CHAT_STREAM: i32 = 3;
pub(crate) const MAGIC_RESPONSES_STREAM: i32 = 4;
pub(crate) const MAGIC_RESPONSES_ONCE: i32 = 5;

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, ScopedContext};

const MOD_AI_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        JSLlmClient::CLASS_NAME,
        JSLlmClient::class_id,
        JS_PROP_ENUMERABLE,
    ),
    FieldDef::CLASS(
        JSSystem1Client::CLASS_NAME,
        JSSystem1Client::class_id,
        JS_PROP_ENUMERABLE,
    ),
    // Question builders, mirroring the TypeSafe SDK's `choice`/`score`/`noul`.
    FieldDef::FUNC("choice", 2, js_question_choice, 0),
    FieldDef::FUNC("score", 2, js_question_score, 0),
    FieldDef::FUNC("noul", 2, js_question_noul, 0),
];

pub fn init_mod_ai(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.use_classes(&[JSLlmClient::class_id(), JSSystem1Client::class_id()])?;
    ctx.create_module("ai", MOD_AI_ENTRIES)?;
    Ok(())
}
