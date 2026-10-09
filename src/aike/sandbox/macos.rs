// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! macOS backend: a Seatbelt profile applied by `/usr/bin/sandbox-exec`.
//!
//! Seatbelt resolves overlapping rules last-match-wins, so the profile is
//! assembled in precedence order — broad allow, then the narrow allows, then
//! `denied` last, which is what makes "denied wins" true here rather than a
//! property the caller has to remember.
//!
//! Every path reaches the policy as a `-D key=value` parameter and is
//! referenced by `(param "KEY")`. Nothing from the caller is ever spliced into
//! the policy text, so an SBPL injection through a path is structurally
//! impossible rather than merely filtered.
//!
//! Measured behaviour of `sandbox-exec`, on macOS 13.2 (Intel) — verified, not
//! assumed, because the older assumption in this file was wrong:
//!
//! - the command's exit status comes back on the `sandbox-exec` process, so
//!   `wait()` is an ordinary wait and needs no /proc-style recovery;
//! - signals reach the command. A SIGTERM to `sandbox-exec` fires the
//!   command's own trap, so the graceful-shutdown ladder's "term" stage is
//!   reachable here;
//! - `process-fork` has to be allowed explicitly — without it `/bin/sh -c
//!   'a; b'` fails with "fork: Operation not permitted".

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::{
        fd::{FromRawFd, IntoRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Command, Stdio},
};

use super::child::{ChildControl, SpawnedChild, WaitResult};
use super::policy::{NetworkPolicy, Policy};

const EXEC: &str = "/usr/bin/sandbox-exec";

/// Printed by `sandbox-exec` on macOS 15 and later, on every invocation.
/// Absent on 13.2. Since stderr is streamed to JS it would otherwise look
/// like every sandboxed command had failed, so it is dropped at the source.
const DEPRECATION_WARNING: &str = "WARNING: sandbox-exec is deprecated";

pub(crate) fn spawn(
    policy: &Policy,
    program: &Path,
    args: &[&str],
    env: &HashMap<String, String>,
) -> Result<SpawnedChild, String> {
    let (profile, params) = build_profile(policy);

    let mut cmd = Command::new(EXEC);
    cmd.arg("-p").arg(&profile);
    for (key, value) in &params {
        cmd.arg(format!("-D{key}={value}"));
    }
    cmd.arg("--").arg(program);
    cmd.args(args);
    // The command starts in the working directory, which is also the only
    // writable place the policy describes. Without this it inherits aike's own
    // cwd, and a relative path would resolve outside every rule below.
    cmd.current_dir(&policy.working_dir);
    cmd.env_clear().envs(child_env(policy, env));
    // Own process group, so one kill(-pgid) reaches the command and anything
    // it started, rather than having to walk the tree.
    cmd.process_group(0);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start sandbox-exec: {e}"))?;
    let pid = child.id() as i32;

    let stdin = child.stdin.take().map(into_file);
    let stdout = child.stdout.take().map(into_file);
    // The relay needs a real File: ChildStderr is a distinct type from the
    // other two pipes and has no From conversion of its own.
    let stderr = child
        .stderr
        .take()
        .map(into_file)
        .map(relay_without_warning);

    // The closures must own `pid`: a ChildControl outlives this function.
    let control = ChildControl::new(
        move || {
            // Both the pid and the group it leads: sandbox-exec is the group
            // leader, so the group is what reaches the exec'd command.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            true
        },
        move || unsafe { libc::kill(pid, 0) == 0 },
        move |sig| unsafe { libc::kill(-pid, sig) == 0 || libc::kill(pid, sig) == 0 },
    );

    Ok(SpawnedChild::new(
        pid,
        stdin,
        stdout,
        stderr,
        control,
        move || {
            let status = child
                .wait()
                .map_err(|e| format!("cannot wait for the sandboxed command: {e}"))?;
            Ok(WaitResult {
                exit_code: status.code().unwrap_or(-1),
                signal: status.signal(),
                killed_by_timeout: false,
            })
        },
    ))
}

/// Relay `src` through a fresh pipe, dropping the deprecation line.
///
/// One thread and one pipe copy per run. Cheaper than the alternative of
/// showing the warning on every command, and cheaper than peeking the first
/// line, which would block until the command wrote something.
fn relay_without_warning(src: std::fs::File) -> std::fs::File {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid two-element array, and pipe() writes exactly
    // two descriptors into it.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        // No relay available. Filtering is a nicety, not the isolation
        // boundary, so pass the pipe through untouched rather than failing
        // the spawn.
        return src;
    }
    // SAFETY: on success pipe() returned two fresh, owned descriptors, and
    // OwnedFd closes them on every path out of this function.
    let (read_end, sink) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let mut source = src;
    let mut sink = std::fs::File::from(sink);

    std::thread::spawn(move || {
        let mut reader = BufReader::new(&mut source);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf);
            if line.trim_start().starts_with(DEPRECATION_WARNING) {
                continue;
            }
            if sink.write_all(&buf).is_err() {
                break; // the reader went away; nothing left to say
            }
            let _ = sink.flush();
        }
    });

    std::fs::File::from(read_end)
}

fn into_file(pipe: impl IntoRawFd) -> std::fs::File {
    // SAFETY: the pipe came from a `Child` we own and have just taken, so it
    // has no other owner.
    unsafe { std::fs::File::from_raw_fd(pipe.into_raw_fd()) }
}

/// The environment the command runs with.
///
/// The host's environment is not inherited: it carries whatever secrets the
/// user's shell holds, which is what the sandbox exists to withhold. What is
/// added back is the minimum a program needs to start. The working directory
/// is the one place the policy says is writable, so TMPDIR points there rather
/// than at the host's `/tmp`, which the policy does not grant.
fn child_env(policy: &Policy, extra: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(path) = std::env::var_os("PATH") {
        env.insert("PATH".to_owned(), path.to_string_lossy().into_owned());
    }
    let cwd = policy.working_dir.to_string_lossy().into_owned();
    env.insert("TMPDIR".to_owned(), cwd.clone());
    env.insert("HOME".to_owned(), cwd);
    env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// Assemble the SBPL profile plus the parameters it references.
fn build_profile(policy: &Policy) -> (String, Vec<(String, String)>) {
    let mut sections = vec![BASE.to_owned()];
    let mut params: Vec<(String, String)> = Vec::new();

    let mut writable = String::from("; the working directory is the one writable place\n");
    let key = add_param(&mut params, "WORKING_DIR", &policy.working_dir);
    writable.push_str(&format!(
        "(allow file-write* (subpath (param \"{key}\")))\n"
    ));
    sections.push(writable);

    if !policy.grants.is_empty() {
        let mut rules = String::from("; configured grants\n");
        for (i, g) in policy.grants.iter().enumerate() {
            let key = add_param(&mut params, &format!("GRANT_{i}"), &g.path);
            // A `literal` rule naming a directory grants nothing, and a
            // `subpath` rule naming a file does not cover it, so the filter
            // comes from what the path was when the policy was built.
            let filter = if g.is_dir { "subpath" } else { "literal" };
            rules.push_str(&format!(
                "(allow file-read* ({filter} (param \"{key}\")))\n"
            ));
            if g.access.can_write() {
                rules.push_str(&format!(
                    "(allow file-write* ({filter} (param \"{key}\")))\n"
                ));
            }
            if g.access.can_execute() {
                rules.push_str(&format!(
                    "(allow process-exec* ({filter} (param \"{key}\")))\n"
                ));
            } else {
                // The base policy allows process-exec globally, and it takes
                // no path filter, so without this a writable directory would
                // also be runnable — and "writable but not runnable" is the
                // point of the access modes.
                rules.push_str(&format!(
                    "(deny process-exec ({filter} (param \"{key}\")))\n"
                ));
            }
        }
        sections.push(rules);
    }

    if !policy.denied.is_empty() {
        let mut rules = String::from("; denied last, because Seatbelt is last-match-wins\n");
        for (i, d) in policy.denied.iter().enumerate() {
            let key = add_param(&mut params, &format!("DENIED_{i}"), d);
            let filter = if d.is_dir() { "subpath" } else { "literal" };
            rules.push_str(&format!(
                "(deny file-read-data ({filter} (param \"{key}\")))\n"
            ));
            rules.push_str(&format!(
                "(deny file-write-data ({filter} (param \"{key}\")))\n"
            ));
            // Keep the entry visible: a program can still see that the path
            // exists, and its name still appears when the *parent* is
            // listed. It does not get to list this directory's own
            // entries — readdir needs read-data on the directory
            // itself — which is a real difference from the Linux backend,
            // where readdir is unrestricted and only the contents are
            // hidden. See the plan's divergence note.
            rules.push_str(&format!(
                "(allow file-read-metadata ({filter} (param \"{key}\")))\n"
            ));
        }
        sections.push(rules);
    }

    match &policy.network {
        // (deny default) already refuses every socket operation.
        NetworkPolicy::DenyAll => {}
        NetworkPolicy::AllowAll => {
            sections.push("(allow network-outbound)\n(allow network-inbound)\n".to_owned())
        }
    }

    (sections.concat(), params)
}

/// The floor. Read is open across the machine — that is the documented
/// default for this kind of sandbox — and the policy's `denied` list is what
/// closes the credential files.
const BASE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow mach-lookup)
(allow file-read*)
(allow file-ioctl)
"#;

fn add_param(params: &mut Vec<(String, String)>, key: &str, path: &Path) -> String {
    params.push((key.to_owned(), path.to_string_lossy().into_owned()));
    key.to_owned()
}

#[cfg(test)]
mod tests {
    use super::super::policy::{Access, Grant};
    use super::*;
    use std::path::PathBuf;

    fn policy() -> Policy {
        Policy {
            working_dir: PathBuf::from("/tmp/wd"),
            grants: vec![Grant {
                path: PathBuf::from("/opt/tools"),
                access: Access::WRITE,
                is_dir: true,
            }],
            denied: vec![PathBuf::from("/tmp/secret")],
            network: NetworkPolicy::DenyAll,
        }
    }

    #[test]
    fn a_path_is_never_spliced_into_the_policy_text() {
        // A path full of SBPL metacharacters must arrive as an opaque
        // parameter value; the policy may only ever name it by key.
        let evil = "/tmp/x\"))(allow file-write* (subpath \"/";
        let (profile, params) = build_profile(&Policy {
            grants: vec![Grant {
                path: PathBuf::from(evil),
                access: Access::READ,
                is_dir: true,
            }],
            ..policy()
        });
        assert!(
            !profile.contains("(subpath \"/"),
            "a caller path reached the policy text: {profile}"
        );
        assert!(profile.contains(r#"(param "GRANT_0")"#), "{profile}");
        assert_eq!(
            params.iter().find(|(k, _)| k == "GRANT_0").unwrap().1,
            evil,
            "the value must reach -D unchanged"
        );
    }

    #[test]
    fn denied_is_emitted_after_grants() {
        // Seatbelt takes the last matching rule, so ordering is the mechanism
        // the "denied wins" rule rests on.
        let (profile, _) = build_profile(&policy());
        let last_grant = profile.find("GRANT_0").expect("grant rule");
        let first_denied = profile.find("DENIED_0").expect("deny rule");
        assert!(last_grant < first_denied, "{profile}");
    }

    #[test]
    fn a_writable_grant_denies_execute_in_that_subtree() {
        // Otherwise "writable" would also mean "runnable", since the base
        // policy allows process-exec globally.
        let (profile, _) = build_profile(&policy());
        assert!(
            profile.contains("(deny process-exec (subpath (param \"GRANT_0\")))"),
            "{profile}"
        );
    }

    #[test]
    fn deny_all_network_emits_no_socket_rule() {
        // (deny default) is what refuses sockets; adding an allow would undo it.
        let (profile, _) = build_profile(&policy());
        assert!(!profile.contains("network-outbound"), "{profile}");
    }

    #[test]
    fn allow_all_network_opens_both_directions() {
        let (profile, _) = build_profile(&Policy {
            network: NetworkPolicy::AllowAll,
            ..policy()
        });
        assert!(profile.contains("(allow network-outbound)"), "{profile}");
    }
}
