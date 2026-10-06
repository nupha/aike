// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Module specifier normalization and module loading.
//!
//! The engine calls [`js_module_normalizer`] to turn an import specifier into
//! the URL the module is cached and compiled under, and [`js_module_loader`] to
//! turn that URL into a [`ModuleDef`]. Bare specifiers (`mymod`, `vendor/lib`)
//! are resolved against a directory search — see [`module_search_dirs`] for the
//! priority order; everything else keeps the engine's default base-relative
//! resolution.

use std::path::{Path, PathBuf};

use zuqe::{JSValue, ModuleDef, ScopedContext};

use crate::transpile;

/// Read file with JS → TS fallback.
/// If `foo.js` not found, tries `foo.ts`. Same for `.mjs` → `.mts`, `.cjs` → `.cts`.
/// Returns (content bytes, resolved filename).
pub(crate) fn read_script(file_name: &str) -> Result<(Vec<u8>, String), String> {
    // Try exact path first
    if let Ok(buf) = std::fs::read(file_name) {
        return Ok((buf, file_name.to_string()));
    }

    // Try TS/MTS/CTS fallback
    if let Some(ts_name) = ts_fallback(file_name)
        && let Ok(buf) = std::fs::read(&ts_name)
    {
        return Ok((buf, ts_name));
    }

    // Report original error
    match std::fs::read(file_name) {
        Ok(buf) => Ok((buf, file_name.to_string())),
        Err(e) => Err(format!("cannot load '{file_name}': {e}")),
    }
}

/// Module name normalizer: resolves bare specifiers (e.g. `mymod`,
/// `vendor/lib`) via the (prefix default, importer dir, CWD, AIKE_LIB_PATH)
/// search; everything else keeps the engine's default base-relative resolution.
///
/// The search runs here (not in the loader) so the module is compiled under
/// the real file URL: that keeps cache identity stable across repeated
/// imports, makes `import.meta.url` point at the actual file, lets relative
/// imports inside the module resolve correctly, and lets the loader dispatch
/// searched `.aike` hits to `load_zapi_extension`.
pub(crate) fn js_module_normalizer(
    _: &mut ScopedContext,
    base_name: &url::Url,
    name: &str,
) -> Result<url::Url, i32> {
    if is_bare_specifier(name) {
        let importer_dir = zuqe::url_to_abspath(base_name)
            .and_then(|p| p.parent().map(|parent| parent.to_path_buf()));
        let dirs = module_search_dirs(importer_dir.as_deref());

        if let Some(path) = search_module(name, &dirs) {
            return zuqe::path_to_url(&path.to_string_lossy()).map_err(|_| -1);
        }
    }

    if Path::new(name).is_absolute() {
        zuqe::path_to_url(name).map_err(|_| -1)
    } else {
        base_name.join(name).map_err(|_| -1)
    }
}

pub(crate) fn js_module_loader(
    ctx: &mut ScopedContext,
    file_name: &url::Url,
    attrs: JSValue,
) -> Result<ModuleDef, i32> {
    // Convert URL back to absolute path for filesystem access.
    let Some(file_path) = zuqe::url_to_abspath(file_name) else {
        return Err(ctx.throw_reference_error(&format!(
            "could not load module '{file_name}': not a file:// URL"
        )));
    };

    let is_native_lib = file_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "aike");

    if is_native_lib {
        return ctx.load_zapi_extension(&file_path);
    }

    let attr_type = get_attr_type(ctx, attrs);

    // Use resolved filename (may be .ts fallback from .js)
    let file_path_str = file_path.as_os_str().to_str().unwrap();
    let resolved = read_script(file_path_str);
    let (buf, resolved_name) = match resolved {
        Ok((b, n)) => (b, n),
        Err(e) => {
            return Err(
                ctx.throw_reference_error(&format!("could not load module '{file_name}'. {e}"))
            );
        }
    };

    // `resolved_name` is the path actually read from disk; it differs from
    // `file_path_str` only when a `.js` import falls back to `.ts` (or
    // `.mjs`→`.mts` / `.cjs`→`.cts`). That fallback is expected and legal —
    // the canonical module name stays `file_name` (used below in
    // `compile_module`), so `resolved_name` must never be used as the key.
    // Assert agreement only when no fallback happened, which still guards
    // against accidentally using `resolved_name` as the canonical name.
    #[cfg(debug_assertions)]
    if ts_fallback(file_path_str).as_deref() != Some(resolved_name.as_str()) {
        debug_assert_eq!(resolved_name, file_path_str);
    }

    // Import-attribute `type` dispatch (mirrors zjs): `json` / `text` wrap the
    // content as a default export; any other type errors; `None` falls through
    // to normal compilation.
    match attr_type.as_deref() {
        Some("json") => {
            // JSON module: wrap as `export default <json>;\n`
            let content = String::from_utf8_lossy(&buf);
            let mut src = String::from("export default ");
            src.push_str(&content);
            src.push('\n');
            ctx.compile_module(src.as_bytes(), file_name, false)
        }
        Some("text") => {
            // Text module: wrap as `export default "<escaped>";\n`
            let content = String::from_utf8_lossy(&buf);
            let mut src = String::from("export default \"");
            src.push_str(&escape_js_string(&content));
            src.push_str("\";\n");
            ctx.compile_module(src.as_bytes(), file_name, false)
        }
        Some(a) => Err(ctx.throw_type_error(&format!("unsupported import attribute type '{a}'"))),
        None => {
            let code = if transpile::needs_transpile(&resolved_name) {
                if resolved_name.ends_with(".d.ts") {
                    return ctx.compile_module(b"export {}", file_name, false);
                }

                let source = String::from_utf8_lossy(&buf);
                match transpile::transpile(&source) {
                    Ok(result) => result.code.into_bytes(),
                    Err(e) => {
                        return Err(ctx.throw_syntax_error(&format!(
                            "failed to transpile module '{resolved_name}': {e}"
                        )));
                    }
                }
            } else {
                buf.to_vec()
            };

            // Note: should use original file URL (not resolved path) as module name
            ctx.compile_module(&code, file_name, false)
        }
    }
}

/// Extract the `type` value from import attributes, returning `None` if
/// the attrs object is absent or has no `type` property.
fn get_attr_type(ctx: &mut ScopedContext, attrs: JSValue) -> Option<String> {
    if !attrs.is_object() {
        return None;
    }
    attrs.get_property_str(ctx, "type").ok().and_then(|v| {
        v.as_string(true)
            .map(|s| s.content().encode_utf8(false).to_string())
    })
}

/// A bare specifier: not empty, not relative (`./x`, `../x`), not absolute,
/// not a URL — only these go through the search-path resolution.
fn is_bare_specifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !is_absolute_specifier(name)
        && url::Url::parse(name).is_err()
}

/// Rooted or drive-qualified, in the sense that matters for an import.
///
/// `Path::is_absolute` answers the *native* question, and the two disagree on
/// Windows: a rooted path there needs a drive letter, so `/abs/mod.js` and
/// `\abs\mod.js` are rejected even though a Windows user means them as
/// absolute. Treating those as bare sends an absolute path into the search
/// path, where `dir.join(name)` resolves it against each directory instead —
/// silently finding the wrong file, or nothing, with no diagnostic.
fn is_absolute_specifier(name: &str) -> bool {
    if Path::new(name).is_absolute() {
        return true;
    }
    // Windows only. On Unix a leading `/` already satisfies the check above,
    // and a leading `\` is an ordinary filename character, not a separator.
    cfg!(windows) && (name.starts_with('/') || name.starts_with('\\'))
}

/// Directories searched for bare specifiers, highest priority first:
///
/// 1. the distribution modules dir derived from the executable path — the
///    extension set shipped with *this* binary, so it must not be shadowable by
///    whatever happens to sit in the project or the CWD (same tier as Node's
///    core modules and Python's standard library, both of which outrank the
///    installed-package directories);
/// 2. the importing module's directory, so module-local deps are not hijacked
///    by the CWD;
/// 3. the CWD;
/// 4. `$AIKE_LIB_PATH` (single directory, skipped when unset or empty) — the
///    weakest, since a planted CWD can forge it via the auto-loaded `.env`.
fn module_search_dirs(importer_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(4);
    if let Some(dir) = dist_modules_dir() {
        dirs.push(dir);
    }
    if let Some(dir) = importer_dir {
        dirs.push(dir.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    if let Ok(lib) = std::env::var("AIKE_LIB_PATH")
        && !lib.is_empty()
    {
        dirs.push(PathBuf::from(lib));
    }
    dirs
}

/// Distribution-layout modules directory: `<prefix>/lib/aike/modules`.
/// where <prefix> is ../../ of the aike executable path.
fn dist_modules_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let prefix = exe.parent()?.parent()?;
    Some(prefix.join("lib").join("aike").join("modules"))
}

/// Resolve a bare specifier against `dirs`, returning the first existing file.
///
/// Per directory (in order): if the joined path is a directory (dotted names
/// like `abc.def` can be directories too), look for `index.js` / `index.ts` /
/// `index.aike` (native zapi extension) inside it; otherwise try `name.js`,
/// `name.ts`, `name.aike`, in that order.
///
/// The first hit wins; a directory without index files falls through to the
/// next directory. `None` means not found in any directory.
fn search_module(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    // Extension is taken from the last path component so dotted directories
    // (e.g. `my.pkg/mod`) don't make the specifier look extension-terminated.
    let has_ext = Path::new(name)
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.rsplit_once('.').is_some_and(|(_, e)| !e.is_empty()));

    for dir in dirs {
        let cand = dir.join(name);
        // dir check first: `abc.def/` is a valid directory name
        if cand.is_dir() {
            for f in ["index.js", "index.ts", "index.aike"] {
                let p = cand.join(f);
                if p.is_file() {
                    return Some(p);
                }
            }
            continue;
        }

        if has_ext {
            if cand.is_file() {
                return Some(cand);
            }
        } else {
            for ext in ["js", "ts", "aike"] {
                let p = cand.with_extension(ext);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    None
}

/// Escape a string for embedding inside a JS double-quoted string literal.
fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x00'..='\x1f' => out.push_str(&format!("\\x{:02x}", ch as u8)),
            c => out.push(c),
        }
    }
    out
}

/// Map `.js` → `.ts`, `.mjs` → `.mts`, `.cjs` → `.cts`
fn ts_fallback(file_name: &str) -> Option<String> {
    if let Some(base) = file_name.strip_suffix(".js") {
        Some(format!("{}.ts", base))
    } else if let Some(base) = file_name.strip_suffix(".mjs") {
        Some(format!("{}.mts", base))
    } else {
        file_name
            .strip_suffix(".cjs")
            .map(|base| format!("{}.cts", base))
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    /// Self-cleaning scratch dir; `write(rel)` creates parent dirs.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "aike_search_{}_{}_{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn write(&self, rel: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        fn mkdir(&self, rel: &str) {
            std::fs::create_dir_all(self.0.join(rel)).unwrap();
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn find(name: &str, dirs: &[&TempDir]) -> Option<PathBuf> {
        let owned: Vec<PathBuf> = dirs.iter().map(|d| d.0.clone()).collect();
        search_module(name, &owned)
    }

    #[test]
    fn finds_js_extension() {
        let d = TempDir::new("js");
        d.write("foo.js");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.js")));
    }

    #[test]
    fn js_beats_ts_beats_aike() {
        let d = TempDir::new("order1");
        d.write("foo.js");
        d.write("foo.ts");
        d.write("foo.aike");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.js")));

        let d = TempDir::new("order2");
        d.write("foo.ts");
        d.write("foo.aike");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.ts")));

        let d = TempDir::new("order3");
        d.write("foo.aike");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.aike")));

        // raw platform suffixes are not module candidates
        let d = TempDir::new("order4");
        d.write("foo.dylib");
        d.write("foo.so");
        d.write("foo.dll");
        assert_eq!(find("foo", &[&d]), None);
    }

    #[test]
    fn dir_uses_index_files() {
        let d = TempDir::new("index1");
        d.write("pkg/index.js");
        d.write("pkg/index.ts");
        d.write("pkg.js");
        // directory wins over sibling extension candidates
        assert_eq!(find("pkg", &[&d]), Some(d.0.join("pkg/index.js")));

        let d = TempDir::new("index2");
        d.write("pkg/index.ts");
        assert_eq!(find("pkg", &[&d]), Some(d.0.join("pkg/index.ts")));
    }

    #[test]
    fn dir_uses_index_aike() {
        // distribution layout: <prefix>/lib/aike/modules/<name>/index.aike
        let d = TempDir::new("pkgai");
        d.write("sqlite/index.aike");
        assert_eq!(find("sqlite", &[&d]), Some(d.0.join("sqlite/index.aike")));

        // index.js / index.ts still outrank index.aike
        let d = TempDir::new("pkgaio");
        d.write("pkg/index.ts");
        d.write("pkg/index.aike");
        assert_eq!(find("pkg", &[&d]), Some(d.0.join("pkg/index.ts")));
    }

    #[test]
    fn dir_without_index_falls_through_to_next_dir() {
        let d1 = TempDir::new("fall1");
        d1.mkdir("pkg");
        let d2 = TempDir::new("fall2");
        d2.write("pkg.js");
        assert_eq!(find("pkg", &[&d1, &d2]), Some(d2.0.join("pkg.js")));
    }

    #[test]
    fn extension_name_is_literal_only() {
        let d = TempDir::new("lit1");
        d.write("bar.js");
        assert_eq!(find("bar.js", &[&d]), Some(d.0.join("bar.js")));

        // has extension → no .ts fallback in search (loader fallback covers that)
        let d = TempDir::new("lit2");
        d.write("bar.ts");
        assert_eq!(find("bar.js", &[&d]), None);
    }

    #[test]
    fn first_dir_wins() {
        let d1 = TempDir::new("first1");
        let d2 = TempDir::new("first2");
        d2.write("foo.js");
        assert_eq!(find("foo", &[&d1, &d2]), Some(d2.0.join("foo.js")));
        d1.write("foo.ts");
        assert_eq!(find("foo", &[&d1, &d2]), Some(d1.0.join("foo.ts")));
    }

    #[test]
    fn subpath_specifier() {
        let d = TempDir::new("sub1");
        d.write("vendor/lib.js");
        assert_eq!(find("vendor/lib", &[&d]), Some(d.0.join("vendor/lib.js")));

        let d = TempDir::new("sub2");
        d.write("vendor/lib/index.js");
        assert_eq!(
            find("vendor/lib", &[&d]),
            Some(d.0.join("vendor/lib/index.js"))
        );
    }

    #[test]
    fn dotted_dir_name_gets_index_lookup() {
        let d = TempDir::new("dotdir");
        d.write("abc.def.js");
        d.write("abc.def/index.js");
        // directory check wins over the sibling extension candidate
        assert_eq!(find("abc.def", &[&d]), Some(d.0.join("abc.def/index.js")));
    }

    #[test]
    fn dotted_dir_component_is_not_extension() {
        let d = TempDir::new("dot1");
        d.write("my.pkg/mod.js");
        assert_eq!(find("my.pkg/mod", &[&d]), Some(d.0.join("my.pkg/mod.js")));

        // "my.pkg" itself ends with an extension → literal lookup
        let d = TempDir::new("dot2");
        d.write("my.pkg");
        assert_eq!(find("my.pkg", &[&d]), Some(d.0.join("my.pkg")));
    }

    #[test]
    fn not_found() {
        let d = TempDir::new("none1");
        d.mkdir("pkg");
        assert_eq!(find("missing", &[&d]), None);
        assert_eq!(find("pkg", &[&d]), None); // dir without index files
    }

    #[test]
    fn bare_specifier_detection() {
        assert!(is_bare_specifier("mymod"));
        assert!(is_bare_specifier("vendor/lib"));
        assert!(is_bare_specifier("mylib.dylib"));
        assert!(!is_bare_specifier("./mymod"));
        assert!(!is_bare_specifier("../mymod"));
        assert!(!is_bare_specifier("/abs/mymod.js"));
        assert!(!is_bare_specifier("file:///abs/mymod.js"));
        assert!(!is_bare_specifier(""));
    }

    /// A rooted path is absolute on every platform. On Windows `is_absolute`
    /// alone rejects `/abs/mod.js` for want of a drive letter, which used to
    /// send it into the search path and resolve it as `<dir>/abs/mod.js`.
    #[test]
    fn rooted_paths_are_never_bare() {
        assert!(is_absolute_specifier("/abs/mod.js"));
        assert!(!is_bare_specifier("/abs/mod.js"));

        if cfg!(windows) {
            // The separator and the drive-qualified form are Windows-only.
            assert!(is_absolute_specifier(r"\abs\mod.js"));
            assert!(!is_bare_specifier(r"\abs\mod.js"));
            assert!(is_absolute_specifier(r"C:\abs\mod.js"));
            assert!(!is_bare_specifier(r"C:\abs\mod.js"));
            // Drive-*relative* (`C:mod.js`) is not rooted: it resolves against
            // that drive's current directory, so it is a path, not a bare name.
            assert!(!is_absolute_specifier(r"C:mod.js"));
        }

        // A leading dot is a relative marker even when it looks rooted.
        assert!(!is_bare_specifier("./mod.js"));
        assert!(!is_absolute_specifier("./mod.js"));
    }

    #[test]
    fn default_modules_dir_follows_dist_layout() {
        // The test binary lives at `<target>/debug/deps/<bin>`, so a grandparent
        // always exists here (unlike a root-level install).
        let dir = dist_modules_dir().expect("prefix derivable from test exe path");
        let tail: Vec<&str> = dir
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_str().unwrap())
            .collect();
        assert_eq!(tail, ["modules", "aike", "lib"]);
    }

    #[test]
    fn search_path_is_prefix_importer_cwd_then_lib_env() {
        let prefix = dist_modules_dir().expect("prefix derivable from test exe path");
        let importer = Path::new("/some/importer/dir");
        let dirs = module_search_dirs(Some(importer));

        // The shipped extension set wins outright: nothing in the project or
        // the CWD may shadow a module that ships with this exact binary.
        assert_eq!(dirs[0], prefix);

        let cwd = std::env::current_dir().unwrap();
        assert!(
            dirs.iter().position(|d| d == importer) < dirs.iter().position(|d| *d == cwd),
            "importer dir must outrank CWD: {dirs:?}"
        );

        // $AIKE_LIB_PATH is the last resort when it is set at all.
        if let Ok(lib) = std::env::var("AIKE_LIB_PATH")
            && !lib.is_empty()
        {
            assert_eq!(dirs.last().unwrap(), &PathBuf::from(lib));
        }
    }
}
