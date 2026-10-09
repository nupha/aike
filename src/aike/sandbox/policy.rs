// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! The sandbox policy: what a sandboxed process may do, independent of any
//! backend.
//!
//! One shape across all platforms. Each backend translates it into its own
//! native form — SID ACL entries on Windows, SBPL text on macOS, `bwrap`
//! arguments on Linux — but none of them changes what the policy means.
//!
//! Paths here are already normalized ([`crate::aike::sandbox::path`]), and the
//! precedence rules are fixed: `denied` beats every grant, and the working
//! directory is always read-write. Both are enforced here, in [`Policy::new`],
//! so a backend cannot accidentally resolve them differently.

use std::path::PathBuf;

use super::path;

/// What the sandboxed process may do with a granted path.
///
/// Write and execute both include read: opening a file in order to write it
/// is itself a read, and mapping one in order to run it is too. No backend can
/// express a path that is writable or executable but not readable, so this
/// type does not offer that shape either — a mode string without `r` is a
/// mistake rather than a narrower grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Access(u8);

impl Access {
    /// Read the path, and list it when it is a directory.
    pub(crate) const READ: Self = Self(1 << 0);
    /// Write the path, and create or remove entries under a directory.
    /// Implies [`Access::READ`]. Without execute, this is what stops the
    /// process from dropping a payload in a directory it can write and then
    /// running it.
    pub(crate) const WRITE: Self = Self(1 << 0 | 1 << 1);
    /// Run the path. Implies [`Access::READ`].
    pub(crate) const EXEC: Self = Self(1 << 0 | 1 << 2);

    /// Parse `r`, `rw`, `rx` or `rwx`. Each letter at most once, and read is
    /// mandatory.
    pub(crate) fn parse(mode: &str) -> Result<Self, String> {
        let mut bits = 0u8;
        for letter in mode.chars() {
            let bit = match letter {
                'r' => Self::READ.0,
                'w' => Self::WRITE.0 & !Self::READ.0,
                'x' => Self::EXEC.0 & !Self::READ.0,
                other => {
                    return Err(format!(
                        "unknown access letter `{other}` in `{mode}`: expected `r`, `w` or `x`"
                    ));
                }
            };
            if bits & bit != 0 {
                return Err(format!("access letter `{letter}` is repeated in `{mode}`"));
            }
            bits |= bit;
        }
        if bits & Self::READ.0 == 0 {
            return Err(format!(
                "access mode `{mode}` does not grant read: write and execute imply it, \
                 so this is a mistake rather than a narrower grant"
            ));
        }
        Ok(Self(bits))
    }

    pub(crate) fn can_write(self) -> bool {
        self.0 & (Self::WRITE.0 & !Self::READ.0) != 0
    }

    pub(crate) fn can_execute(self) -> bool {
        self.0 & (Self::EXEC.0 & !Self::READ.0) != 0
    }
}

impl std::fmt::Display for Access {
    /// The same mode string `parse` accepts, which is what a failing
    /// assertion needs to read.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("r")?;
        if self.can_write() {
            f.write_str("w")?;
        }
        if self.can_execute() {
            f.write_str("x")?;
        }
        Ok(())
    }
}

/// One configured path and what the sandbox may do with it. Grants are
/// additive: a path granted twice keeps the union of both.
#[derive(Clone, Debug)]
pub(crate) struct Grant {
    /// Already normalized.
    pub(crate) path: PathBuf,
    pub(crate) access: Access,
    /// Whether `path` was a directory when the policy was built.
    ///
    /// Recorded rather than re-checked by the backends: a Seatbelt `literal`
    /// rule naming a directory grants nothing at all, and a `subpath` rule
    /// naming a file does not cover it, so getting this wrong makes a grant
    /// silently ineffective. Deciding once, from the path the rest of the
    /// policy reasons about, keeps every backend in agreement.
    pub(crate) is_dir: bool,
}

/// Where outbound connections may go.
///
/// A domain allowlist would be enforced by a proxy running outside the
/// sandbox, which is not built yet. Until then it is refused at parse time
/// rather than accepted and silently downgraded to a total block — see
/// `parse_network` in `js.rs`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NetworkPolicy {
    /// No route out. The default.
    DenyAll,
    /// Anything the host can reach.
    AllowAll,
}

/// A validated policy. The only way to build one is [`Policy::new`], so a
/// backend can rely on every field already being normalized and every
/// contradiction already rejected.
#[derive(Clone, Debug)]
pub(crate) struct Policy {
    /// The directory the process starts in. Always read-write, never
    /// executable, and not expressible as a grant: no configuration changes
    /// it.
    pub(crate) working_dir: PathBuf,
    /// Additional paths, beyond the working directory.
    pub(crate) grants: Vec<Grant>,
    /// Paths the process may not reach at all, whatever else says otherwise.
    pub(crate) denied: Vec<PathBuf>,
    pub(crate) network: NetworkPolicy,
}

impl Policy {
    /// Normalize and validate. `working_dir`, `grants` and `denied` are the
    /// raw, un-normalized paths from the caller.
    ///
    /// Rejects, rather than resolving silently:
    /// - a working directory that does not exist or is not a directory;
    /// - a grant or denied path that does not exist — normalizing one would
    ///   otherwise make its absence invisible and the rule would quietly cover
    ///   nothing;
    /// - a denied path that is the working directory, or that contains a
    ///   granted path: `denied` wins, so the grant would be dead config that
    ///   the caller was not told about.
    pub(crate) fn new(
        working_dir: &str,
        grants: Vec<(String, String)>,
        denied: Vec<String>,
        network: NetworkPolicy,
    ) -> Result<Self, String> {
        let cwd = path::existing_dir(working_dir, "cwd")?;

        let mut normalized_grants: Vec<Grant> = Vec::with_capacity(grants.len());
        for (raw, mode) in grants {
            let access =
                Access::parse(&mode).map_err(|e| format!("`grants` entry for `{raw}`: {e}"))?;
            let p = path::existing(&raw, "grants")
                .map_err(|e| format!("`grants` entry for `{raw}`: {e}"))?;
            // A grant on a file that cannot be executed would forbid running a
            // file the caller explicitly handed over, which is not something
            // they can have meant.
            if access.can_write() && !access.can_execute() && p.is_file() {
                return Err(format!(
                    "`grants` entry for `{raw}` grants write but not execute on a file; \
                     pass \"rwx\" to allow both, or \"r\" to allow neither"
                ));
            }
            normalized_grants.push(Grant {
                is_dir: p.is_dir(),
                path: p,
                access,
            });
        }
        normalized_grants.sort_by(|a, b| a.path.cmp(&b.path));

        let mut normalized_denied = Vec::with_capacity(denied.len());
        for raw in denied {
            let p = path::existing(&raw, "denied")
                .map_err(|e| format!("`denied` entry `{raw}`: {e}"))?;
            normalized_denied.push(p);
        }
        normalized_denied.sort();

        // The working directory is read-write no matter what, so a `denied`
        // that reaches it contradicts itself rather than narrowing anything.
        if let Some(d) = normalized_denied.iter().find(|d| path::is_within(&cwd, d)) {
            return Err(format!(
                "`denied` entry `{}` covers the sandbox `cwd` `{}`; the working directory \
                 is always read-write",
                d.display(),
                cwd.display()
            ));
        }

        // `denied` outranks every grant, so a denied path containing a granted
        // one is dead configuration the caller was not told about.
        for g in &normalized_grants {
            for d in &normalized_denied {
                if path::is_within(&g.path, d) {
                    return Err(format!(
                        "`denied` entry `{}` contains granted path `{}`; `denied` wins, \
                         so the grant would do nothing",
                        d.display(),
                        g.path.display()
                    ));
                }
            }
        }

        Ok(Self {
            working_dir: cwd,
            grants: normalized_grants,
            denied: normalized_denied,
            network,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aike::sandbox::path::testdir::TempDir;

    fn tmp() -> TempDir {
        TempDir::new()
    }

    #[test]
    fn access_parses_the_four_modes() {
        assert_eq!(Access::parse("r").unwrap(), Access::READ);
        assert_eq!(Access::parse("rw").unwrap(), Access::WRITE);
        assert_eq!(Access::parse("rx").unwrap(), Access::EXEC);
        assert_eq!(Access::parse("rwx").unwrap().can_write(), true);
        assert_eq!(Access::parse("rwx").unwrap().can_execute(), true);
        assert!(!Access::parse("rw").unwrap().can_execute());
        assert!(!Access::parse("rx").unwrap().can_write());
    }

    #[test]
    fn access_rejects_a_mode_without_read() {
        // Write and execute imply read, so a mode missing `r` is a mistake,
        // not a narrower grant.
        for mode in ["w", "x", "wx", ""] {
            assert!(Access::parse(mode).is_err(), "{mode} should not parse");
        }
    }

    #[test]
    fn access_rejects_unknown_and_repeated_letters() {
        assert!(Access::parse("rr").is_err());
        assert!(Access::parse("rwr").is_err());
        assert!(Access::parse("rz").is_err());
        assert!(Access::parse("rwx r").is_err());
    }

    #[test]
    fn access_display_round_trips_through_parse() {
        for mode in ["r", "rw", "rx", "rwx"] {
            let a = Access::parse(mode).unwrap();
            assert_eq!(a.to_string(), mode);
        }
    }

    #[test]
    fn policy_requires_an_existing_directory_as_cwd() {
        let dir = tmp();
        assert!(
            Policy::new(
                dir.path().to_str().unwrap(),
                vec![],
                vec![],
                NetworkPolicy::DenyAll
            )
            .is_ok()
        );

        let missing = dir.path().join("nope");
        let err = Policy::new(
            missing.to_str().unwrap(),
            vec![],
            vec![],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("cwd"), "{err}");
    }

    #[test]
    fn policy_rejects_a_cwd_that_is_a_file() {
        let dir = tmp();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let err = Policy::new(
            file.to_str().unwrap(),
            vec![],
            vec![],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn policy_rejects_a_grant_that_does_not_exist() {
        let dir = tmp();
        let ghost = dir.path().join("ghost");
        let err = Policy::new(
            dir.path().to_str().unwrap(),
            vec![(ghost.to_str().unwrap().to_owned(), "rx".to_owned())],
            vec![],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("grants"), "{err}");
    }

    #[test]
    fn policy_rejects_deny_over_a_grant_instead_of_silently_winning() {
        let dir = tmp();
        let secret = dir.path().join("secret");
        std::fs::create_dir(&secret).unwrap();
        let inner = secret.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        let s = |p: &std::path::Path| p.to_str().unwrap().to_owned();

        // denied is the granted path itself.
        let err = Policy::new(
            &s(dir.path()),
            vec![(s(&tools), "rx".to_owned())],
            vec![s(&tools)],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("denied"), "{err}");

        // denied is an ancestor of the granted path.
        let err = Policy::new(
            &s(dir.path()),
            vec![(s(&inner), "rx".to_owned())],
            vec![s(&secret)],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("denied"), "{err}");

        // Siblings do not conflict: a denied path beside a granted one is the
        // normal way to hide one directory inside a workspace.
        assert!(
            Policy::new(
                &s(dir.path()),
                vec![(s(&tools), "rx".to_owned())],
                vec![s(&secret)],
                NetworkPolicy::DenyAll,
            )
            .is_ok()
        );
    }

    #[test]
    fn policy_rejects_deny_over_the_working_directory() {
        let dir = tmp();
        let err = Policy::new(
            dir.path().to_str().unwrap(),
            vec![],
            vec![dir.path().to_str().unwrap().to_owned()],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("cwd"), "{err}");
    }

    #[test]
    fn policy_rejects_write_without_execute_on_a_file() {
        let dir = tmp();
        let file = dir.path().join("data.bin");
        std::fs::write(&file, b"x").unwrap();
        let err = Policy::new(
            dir.path().to_str().unwrap(),
            vec![(file.to_str().unwrap().to_owned(), "rw".to_owned())],
            vec![],
            NetworkPolicy::DenyAll,
        )
        .unwrap_err();
        assert!(err.contains("rwx"), "{err}");
    }
}
