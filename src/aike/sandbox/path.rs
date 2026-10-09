// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Path normalization and matching for sandbox policies.
//!
//! Every path a policy names goes through here first, so that all three
//! backends compare the same bytes. `canonicalize` is what makes the rules
//! hard to route around: `..`, a symlink pointing elsewhere, or a trailing
//! `/tmp/x/..` all collapse to one absolute real path before any backend sees
//! them, and a prefix test on that form cannot be walked out of.

use std::path::{Path, PathBuf};

/// Resolve `path` to an absolute, symlink-free form.
///
/// `component` names the option the path came from, so the message points at
/// the caller's own field rather than at an internal step.
pub(crate) fn normalize(path: &str, component: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if p.as_os_str().is_empty() {
        return Err(format!("`{component}` path must not be empty"));
    }
    // A relative path would otherwise resolve against whatever the runtime's
    // cwd happens to be, which is the caller's `cwd` and not their intent.
    if !p.is_absolute() {
        return Err(format!(
            "`{component}` path `{path}` must be absolute, not relative"
        ));
    }
    p.canonicalize().map_err(|e| {
        // A missing path is the common case and gets the clearer phrasing;
        // matching on the error kind keeps it working under a localized OS.
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{component}` path `{path}` does not exist")
        } else {
            format!("`{component}` path `{path}` cannot be resolved: {e}")
        }
    })
}

/// [`normalize`], phrased for a path the caller expected to exist.
///
/// `canonicalize` already fails when nothing is there, so there is no second
/// existence check to make — only the message to improve, which `normalize`
/// now does for a missing path.
pub(crate) fn existing(path: &str, component: &str) -> Result<PathBuf, String> {
    normalize(path, component)
}

/// [`existing`], plus a check that it is a directory. The sandbox working
/// directory has to be one, or the process cannot be started there.
pub(crate) fn existing_dir(path: &str, component: &str) -> Result<PathBuf, String> {
    let p = existing(path, component)?;
    if !p.is_dir() {
        return Err(format!("`{component}` path `{path}` is not a directory"));
    }
    Ok(p)
}

/// Whether `path` is `root` or lies beneath it.
///
/// Both sides must already be normalized: on a resolved path this is a plain
/// component-wise prefix test, which is what keeps `/tmp/foobar` from matching
/// a rule for `/tmp/foo`.
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// A self-cleaning temporary directory, for tests. Private to `cfg(test)` so it
/// costs nothing in a normal build and aike keeps no dev-dependency for it.
#[cfg(test)]
pub(crate) mod testdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            // pid keeps concurrent test binaries from colliding; the counter
            // keeps concurrent tests inside one binary from colliding.
            let p = std::env::temp_dir().join(format!("aike-sbx-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TempDir;
    use super::*;

    #[test]
    fn rejects_empty_and_relative_paths() {
        assert!(normalize("", "grants").is_err());
        assert!(normalize("relative/path", "grants").is_err());
        assert!(normalize("./x", "grants").is_err());
    }

    #[test]
    fn resolves_dot_dot_before_comparing() {
        let dir = TempDir::new();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let via_dotdot = format!("{}/real/../real", dir.path().display());

        let a = normalize(&via_dotdot, "grants").unwrap();
        let b = normalize(real.to_str().unwrap(), "grants").unwrap();
        assert_eq!(a, b, ".. must collapse before comparison");
    }

    #[test]
    #[cfg(unix)] // symlink creation needs privileges on Windows
    fn resolves_symlinks_before_comparing() {
        let dir = TempDir::new();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let a = normalize(link.to_str().unwrap(), "grants").unwrap();
        let b = normalize(real.to_str().unwrap(), "grants").unwrap();
        assert_eq!(a, b, "a symlink must not grant something its target denies");
    }

    #[test]
    fn a_rule_cannot_be_walked_out_of_with_dot_dot() {
        let dir = TempDir::new();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir(&allowed).unwrap();
        let secret = dir.path().join("secret");
        std::fs::create_dir(&secret).unwrap();

        // A path that lexically starts inside `allowed` but escapes it.
        let escape = format!("{}/../secret", allowed.display());
        let escaped = normalize(&escape, "grants").unwrap();
        // Both sides normalized: on macOS the temp dir is reached through
        // /var, which is a symlink to /private/var, and comparing a resolved
        // path against an unresolved one would fail for the wrong reason.
        let allowed = allowed.canonicalize().unwrap();
        let secret = secret.canonicalize().unwrap();

        assert!(
            !is_within(&escaped, &allowed),
            "{escaped:?} must not be treated as inside {allowed:?}"
        );
        assert!(is_within(&escaped, &secret));
    }

    #[test]
    fn sibling_prefixes_do_not_match() {
        // /tmp/foo must not match a rule for /tmp/foobar.
        let root = Path::new("/tmp/foo");
        assert!(is_within(Path::new("/tmp/foo"), root));
        assert!(is_within(Path::new("/tmp/foo/bar"), root));
        assert!(!is_within(Path::new("/tmp/foobar"), root));
        assert!(!is_within(Path::new("/tmp"), root));
    }

    #[test]
    fn existing_rejects_a_path_that_is_not_there() {
        let dir = TempDir::new();
        let ghost = dir.path().join("ghost");
        let err = existing(ghost.to_str().unwrap(), "grants").unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("grants"), "message names the option: {err}");
    }

    #[test]
    fn existing_dir_rejects_a_file() {
        let dir = TempDir::new();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let err = existing_dir(file.to_str().unwrap(), "cwd").unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }
}
