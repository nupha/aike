// Type declarations for the CommonJS loader — the global `require`, the
// per-module bindings, and `require.cache` / `require.extensions`.
//
// Where aike differs from Node's CommonJS:
//   - `require` is a global, usable from ES module scope (Node: no).
//   - Builtins answer to a bare name; the `node:` prefix is dropped —
//     `require("node:fs")` and `require("fs")` are one module.
//   - `require.resolve.paths()` always returns the module search path; it
//     never answers `null` for a builtin or a non-bare name.
//   - Bare specifiers resolve against the search dirs on {@link Module.paths}
//     — aike has no `node_modules` walk.
//   - `require.extensions` registers only `.js` and `.json`; there is no
//     `.node` addon loader (native extensions load as zapi `.aike` files).
//   - `.ts`, `.cts` and `.aike` files load natively; Node requires a loader.
//
// Everything else matches Node and is described inline without a note.

// ---------------------------------------------------------------------------
// Module record
// ---------------------------------------------------------------------------

/**
 * The `module` object handed to every CommonJS module. Also the value
 * `require.main` points at for the process entry.
 */
interface Module {
  /**
   * `"."` for the process entry, otherwise the absolute path — which for
   * every module except the entry is also `__filename`.
   */
  id: string;
  /**
   * The module that `require`d this one. `null` for the process entry, and
   * `undefined` for a module with no CommonJS parent — reached from an ES
   * module, or the `-e` entry.
   */
  parent: Module | null | undefined;
  /**
   * Modules this one required, appended in load order. Cycles are possible:
   * a module appears here before it finishes loading.
   */
  children: Module[];
  /**
   * Directories searched for a bare specifier, in order: the distribution
   * module directory (`<prefix>/lib/aike/modules`), the requiring module's
   * directory, the CWD, then `$AIKE_LIB_PATH`.
   *
   * Not Node's `node_modules` walk — aike resolves bare specifiers against
   * these directories.
   */
  paths: string[];
  /** Exports object; reassignable, and what `require()` returns. */
  exports: any;
  /** Set to `true` once the module body has finished running. */
  loaded: boolean;
  /** Absolute path of this module — the same string as `__filename`. */
  filename: string;
  /** Directory containing this module — the same string as `__dirname`. */
  path: string;
  /**
   * This module's own `require`, bound to its directory; it is the function
   * passed to the wrapper, so `module.require === require` inside the module.
   *
   * Distinct from the global `require`, which resolves against the calling
   * module's directory instead.
   */
  require: NodeRequire;
}

// ---------------------------------------------------------------------------
// require
// ---------------------------------------------------------------------------

/**
 * `require.resolve` — the path `require()` would take, and the directories it
 * searches for a bare specifier.
 *
 * Not Node's `node_modules` walk; see {@link Module.paths}.
 */
interface RequireResolve {
  /** The absolute path `specifier` would load, without loading it. */
  (specifier: string): string;

  /**
   * Directories searched for a bare specifier, nearest first. This is aike's
   * module search path, returned unchanged for any `specifier` — unlike Node,
   * aike does not single out a non-bare name or return `null` for a builtin.
   */
  paths(specifier: string): string[];
}

/**
 * Load a module.
 *
 * Resolution runs before the filesystem lookup: a bare specifier that names a
 * native module (`fs`, `path`, `os`, …) resolves to that module's namespace
 * object — the same object `import` yields, not a wrapper. A `node:` prefix is
 * stripped first, so `require("node:fs")` and `require("fs")` are one module.
 */
interface NodeRequire {
  /**
   * `specifier` is one of:
   *
   * - a **relative or absolute path** — `./x`, `../x`, `/abs/x`. Tried as a
   *   file first (exact name, then `.js`, `.json`, `.cjs`, `.ts`, `.cts`,
   *   `.aike`), then as a directory (`index` with the same extensions).
   * - a **bare specifier** — `fs`, `some-pkg`. Resolved against the
   *   directories reported by `require.resolve.paths(specifier)`.
   * - a **zapi extension** — `./native.aike` or a bare name whose file is
   *   `.aike`. Loaded by the engine's extension loader; there is no CommonJS
   *   form of an extension.
   *
   * Loads JavaScript and TypeScript modules, JSON files, native modules and
   * zapi extensions. A file with no `import`/`export` runs as CommonJS; one
   * with them runs as an ES module and is returned as its namespace, except
   * that a namespace carrying a `default` export is shallow-copied onto a
   * null-prototype object marked `__esModule: true`, so CJS interop sees the
   * default. The copy is not identity-equal to the `import` namespace, but
   * its values are shared.
   *
   * @throws ReferenceError with `code: "MODULE_NOT_FOUND"` if nothing
   * resolves. The message ends with a `Require stack:` listing the requirers.
   * @throws ERR_REQUIRE_ASYNC_MODULE if the target ES module has top-level
   * await — use `import()` instead.
   */
  (specifier: string): any;

  /**
   * The absolute path `specifier` would load, without loading it. Throws the
   * same `MODULE_NOT_FOUND` as `require`.
   */
  resolve: RequireResolve;

  /**
   * Loaded CommonJS modules, keyed by absolute path. Present even before the
   * first `require`.
   *
   * Only files appear here: native modules are not files, and required ES
   * modules are kept by the ES module registry instead. Deleting a key forces
   * the next `require` of that path to run the file again.
   */
  cache: { [path: string]: Module };

  /**
   * Loaders keyed by extension, as `(module, filename) => void`. Only `.js`
   * and `.json` are registered; replacing an entry changes how that extension
   * loads. An unregistered extension falls back to the `.js` loader.
   *
   * TypeScript is deliberately absent — `.ts`/`.cts` go through the `.js`
   * loader, which strips the types. There is no `.node` addon loader; native
   * extensions load as zapi `.aike` files instead.
   */
  extensions: {
    [extension: string]: (module: Module, filename: string) => void;
  };

  /**
   * The process entry's module record, or `undefined` when there is none
   * (`-e` code, or an ES module entry).
   */
  main: Module | undefined;
}

/**
 * The global `require`.
 *
 * **Not a Node global.** Node has no `require` in ES module scope; aike
 * installs one because scripts are evaluated in global scope and the generated
 * ESM↔CJS interop source calls it. It is not bound to any file: it resolves
 * against the calling module's directory, or the CWD when there is none —
 * Node's `module.createRequire` rule — which is what makes it usable from ES
 * module scope and from `-e` code.
 *
 * Distinct from a module's own `module.require`, which is bound to that
 * module's directory.
 */
declare const require: NodeRequire;

// ---------------------------------------------------------------------------
// Wrapper parameters
// ---------------------------------------------------------------------------

// Unlike `require`, these are parameters of the CommonJS wrapper, not globals:
// they are absent in ES module scope. Declared globally for type-checking, as
// Node's own types do.

/**
 * The running module's record. Reassign `module.exports` to control what
 * `require()` hands back; assigning the bare `exports` binding does not.
 */
declare const module: Module;

/**
 * The initial value of `module.exports` — the same object until `module.exports`
 * is reassigned. Reassigning `exports` itself is a no-op for `require()`.
 */
declare const exports: any;

/**
 * Absolute path of the running CommonJS module — the same string as
 * `module.filename`. Absent (a `ReferenceError`) in an ES module.
 */
declare const __filename: string;

/**
 * Directory of the running CommonJS module — the same string as
 * `module.path`. Absent (a `ReferenceError`) in an ES module.
 */
declare const __dirname: string;
