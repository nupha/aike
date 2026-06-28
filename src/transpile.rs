// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! TypeScript → JavaScript transpiler.
//! Strips type annotations using SWC, no syntax downleveling.
//!
//! Feature flag: typescript (enabled by default)
//! When disabled, transpile() returns an error, needs_transpile() still works normally.

#[cfg(feature = "typescript")]
mod transpile_impl {
    use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
    use swc_ecma_ast::Program;
    use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
    use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
    use swc_ecma_transforms_typescript::strip;

    pub fn transpile(source: &str) -> Result<String, String> {
        let cm: Lrc<SourceMap> = Lrc::new(SourceMap::default());
        let globals = Globals::new();
        GLOBALS.set(&globals, || {
            // Parse TS source into a Module
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
            let module = parser
                .parse_module()
                .map_err(|e| format!("TS parse error: {:?}", e))?;

            // Strip types via SWC Pass
            let unresolved_mark = Mark::fresh(Mark::root());
            let top_level_mark = Mark::fresh(Mark::root());
            let mut pass = strip(unresolved_mark, top_level_mark);
            let program = Program::Module(module).apply(&mut pass);
            let module = match program {
                Program::Module(m) => m,
                _ => unreachable!(),
            };

            // Generate JS code from the stripped module
            let mut buf = Vec::new();
            let mut emitter = Emitter {
                cfg: swc_ecma_codegen::Config::default()
                    .with_target(swc_ecma_ast::EsVersion::Es2022),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm, "\n", &mut buf, None),
            };
            emitter
                .emit_module(&module)
                .map_err(|e| format!("JS generation error: {:?}", e))?;
            String::from_utf8(buf).map_err(|e| e.to_string())
        })
    }
}

#[cfg(not(feature = "typescript"))]
mod transpile_impl {
    /// Stub when feature is disabled — .ts files will error out
    pub fn transpile(_source: &str) -> Result<String, String> {
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
