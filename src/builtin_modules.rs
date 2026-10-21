// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Registry of the built-in modules.
//!
//! A built-in module is installed on demand: the module loader recognizes the
//! synthetic `aike-builtin:<name>` URL and calls the module's install entry
//! point. The root runtime installs them all up front; a child runtime
//! installs only the ones its import policy lets through.
//!
//! The install entry point owns the module's class initialization, so the
//! class list cannot drift away from the module that needs it.

use zuqe::{ModuleDef, ScopedContext};

/// A built-in module and the entry point that installs it.
pub(crate) struct BuiltinModule {
    /// Registration name — what `import "<name>"` resolves to.
    pub(crate) name: &'static str,
    /// Create the module and return its definition.
    pub(crate) install: fn(&mut ScopedContext) -> Result<ModuleDef, i32>,
}

/// Every built-in module, in registration order.
pub(crate) const BUILTIN_MODULES: &[BuiltinModule] = &[
    BuiltinModule {
        name: "fs",
        install: crate::fs::install_mod_fs,
    },
    BuiltinModule {
        name: "path",
        install: crate::path::install_mod_path,
    },
    BuiltinModule {
        name: "os",
        install: crate::os::install_mod_os,
    },
    BuiltinModule {
        name: "http",
        install: crate::http::install_mod_http,
    },
    BuiltinModule {
        name: "html",
        install: crate::html::install_mod_html,
    },
    BuiltinModule {
        name: "yaml",
        install: crate::yaml::install_mod_yaml,
    },
    BuiltinModule {
        name: "jsonl",
        install: crate::jsonl::install_mod_jsonl,
    },
    BuiltinModule {
        name: "json-schema",
        install: crate::json_schema::install_mod_json_schema,
    },
    BuiltinModule {
        name: "jinja2",
        install: crate::jinja2::install_mod_jinja2,
    },
    BuiltinModule {
        name: "ai",
        install: crate::ai::install_mod_ai,
    },
    BuiltinModule {
        name: "mcp",
        install: crate::mcp::install_mod_mcp,
    },
];

/// Look up a built-in module by name.
pub(crate) fn builtin_module(name: &str) -> Option<&'static BuiltinModule> {
    BUILTIN_MODULES.iter().find(|m| m.name == name)
}

/// Whether `name` is a built-in module name.
pub(crate) fn is_builtin_module(name: &str) -> bool {
    builtin_module(name).is_some()
}

/// Install one built-in module.
pub(crate) fn install_builtin(
    ctx: &mut ScopedContext,
    module: &BuiltinModule,
) -> Result<ModuleDef, i32> {
    (module.install)(ctx)
}

/// Install every built-in module — what the root runtime does at startup.
pub(crate) fn install_all(ctx: &mut ScopedContext) -> Result<(), i32> {
    for module in BUILTIN_MODULES {
        install_builtin(ctx, module)?;
    }
    Ok(())
}
