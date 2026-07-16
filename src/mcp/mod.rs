// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_mcp_server_def;

use class_mcp_server_def::JSMCPServerDef;

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, ScopedContext};

const MOD_MCP_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS("MCPServerDef", JSMCPServerDef::class_id, JS_PROP_ENUMERABLE),
    FieldDef::FUNC(
        "defineMCPStdio",
        1,
        class_mcp_server_def::js_define_mcp_stdio,
        0,
    ),
];

pub(crate) fn init_mod_mcp(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.use_classes(&[JSMCPServerDef::class_id()])?;

    ctx.create_module("mcp", MOD_MCP_ENTRIES)?;

    Ok(())
}
