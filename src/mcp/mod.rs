// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_mcp_server_def;

use class_mcp_server_def::JSMcpServerDef;

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, ScopedContext};

pub(crate) fn init_mod_mcp(ctx: &mut ScopedContext) -> Result<(), i32> {
    const MOD_MCP_ENTRIES: &[FieldDef] = &[
        FieldDef::CLASS(
            JSMcpServerDef::CLASS_NAME,
            JSMcpServerDef::class_id,
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::FUNC(
            "defineStdioMcp",
            1,
            class_mcp_server_def::js_define_mcp_stdio,
            0,
        ),
    ];

    ctx.use_classes(&[JSMcpServerDef::class_id()])?;
    ctx.create_module("mcp", MOD_MCP_ENTRIES)?;

    Ok(())
}
