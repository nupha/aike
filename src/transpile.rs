// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! TypeScript → JavaScript transpiler.
//! Strips type annotations using SWC, no syntax downleveling.
//!
//! Uses SWC's `parse_program()` to let the parser decide module vs. script,
//! and reports the result via `TranspileResult.is_module`.
//! Top-level await is inherent to modules — no separate detection needed.
//!
//! Feature flag: typescript (enabled by default)
//! When disabled, transpile() returns an error, needs_transpile() still works normally.

/// Result of transpiling a .ts file.
pub struct TranspileResult {
    /// The transpiled JavaScript code.
    pub code: String,
    /// Whether the code is an ES module (has import/export statements).
    pub is_module: bool,
}

#[cfg(feature = "typescript")]
mod transpile_impl {
    use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
    use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
    use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
    use swc_ecma_transforms_typescript::strip;

    use super::TranspileResult;

    #[inline(never)]
    pub fn transpile(source: &str) -> Result<TranspileResult, String> {
        let cm: Lrc<SourceMap> = Lrc::new(SourceMap::default());
        let globals = Globals::new();

        GLOBALS.set(&globals, || {
            let fm = cm.new_source_file(FileName::Anon.into(), source.to_string());
            let mut parser = Parser::new(
                Syntax::Typescript(TsSyntax {
                    tsx: false,
                    decorators: true,
                    ..Default::default()
                }),
                StringInput::from(&*fm),
                None,
            );
            // parse_program() lets SWC decide module vs. script automatically.
            let program = parser
                .parse_program()
                .map_err(|e| format!("TS parse error: {:?}", e))?;

            let is_module = matches!(program, swc_ecma_ast::Program::Module(_));

            // Strip types via SWC Pass (works on Program, no extra wrapping needed)
            let unresolved_mark = Mark::fresh(Mark::root());
            let top_level_mark = Mark::fresh(Mark::root());
            let mut pass = strip(unresolved_mark, top_level_mark);
            let program = program.apply(&mut pass);

            // Generate JS code (emit_program handles both Module and Script)
            let mut buf = Vec::new();
            let mut emitter = Emitter {
                cfg: swc_ecma_codegen::Config::default()
                    .with_target(swc_ecma_ast::EsVersion::Es2022),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm, "\n", &mut buf, None),
            };
            emitter
                .emit_program(&program)
                .map_err(|e| format!("JS generation error: {:?}", e))?;
            let code = String::from_utf8(buf).map_err(|e| e.to_string())?;

            Ok(TranspileResult { code, is_module })
        })
    }
}

#[cfg(not(feature = "typescript"))]
mod transpile_impl {
    use super::TranspileResult;

    /// Stub when feature is disabled — .ts files will error out
    pub fn transpile(_source: &str) -> Result<TranspileResult, String> {
        Err("TypeScript support not enabled (compiled without typescript feature)".into())
    }
}

/// Check if a filename needs transpilation (not gated by feature flag, always callable)
pub fn needs_transpile(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    (lower.ends_with(".ts") && !lower.ends_with(".d.ts"))
        || lower.ends_with(".mts")
        || lower.ends_with(".cts")
}

pub use transpile_impl::transpile;
