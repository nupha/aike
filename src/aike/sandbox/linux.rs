// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Linux backend: `bwrap`, the unprivileged sandboxer from the flatpak
//! project.
//!
//! Landlock was rejected here: it filters access to paths, but it is not a
//! jail. It cannot give the process a private `/proc`, a PID namespace, or a
//! network namespace, so a process confined by it alone still sees every host
//! process and still has the host's `/dev`. `bwrap` sets all of that up, and it
//! is what Claude Code, Codex, Cursor and flatpak use.
//!
//! ⚠️ **Two places where this backend cannot express what the others do.**
//! Both are inherent to what `bwrap` can do, not oversights:
//!
//! 1. **Writable implies runnable.** `--bind` does not distinguish the execute
//!    bit, so a granted `rw` directory is also `rx` here. The other two
//!    backends separate them. This weakens the "write a payload, run it"
//!    barrier on Linux specifically.
//! 2. **Paths are mounted, not filtered.** Reads are open across the machine
//!    (the root is bound read-only), and a denied path is closed by covering
//!    it: a directory takes a private tmpfs and its contents go away, a file is
//!    bound over with `/dev/null`. In both cases the *name* stays visible and
//!    `stat` succeeds — the same shape macOS gives with `(deny file-read-data)`,
//!    where only the contents are hidden.
//!
//! A third difference is in signal handling, and it is not `bwrap`'s to fix:
//! `bwrap` does not forward signals to the command, so `kill(SIGTERM)` is a
//! hard kill of the whole sandbox and the command's own handler never runs.
//! macOS's `sandbox-exec` does forward. See `Subprocess.kill` in the type
//! declarations.

use std::{
    collections::HashMap,
    os::{
        fd::{FromRawFd, IntoRawFd},
        unix::process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use super::{
    child::{ChildControl, SpawnedChild, WaitResult},
    policy::{NetworkPolicy, Policy},
};

pub(crate) fn spawn(
    policy: &Policy,
    program: &Path,
    args: &[&str],
    env: &HashMap<String, String>,
) -> Result<SpawnedChild, String> {
    let bwrap = locate_bwrap()?;
    let mut cmd = Command::new(&bwrap);
    cmd.args(build_args(policy, program, args)?);
    // Inside the namespace the working directory is the same path it has
    // outside (bwrap binds it at its real location), so this is the cwd the
    // policy's rules were written against.
    cmd.current_dir(&policy.working_dir);
    cmd.env_clear().envs(child_env(policy, env));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bwrap.display()))?;
    let pid = child.id() as i32;

    let stdin = child.stdin.take().map(into_file);
    let stdout = child.stdout.take().map(into_file);
    let stderr = child.stderr.take().map(into_file);

    // The bwrap process becomes the sandbox's pid namespace init and stays
    // alive for exactly as long as the command does, so signals and liveness
    // both work against it directly. The process group additionally reaches
    // anything the command started that stayed outside the namespace.
    //
    // The closures must own `pid`: a ChildControl outlives this function.
    let control = ChildControl::new(
        move || {
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

/// The `bwrap` to run: an explicit override if one is set, otherwise the one on
/// `PATH`.
fn locate_bwrap() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("AIKE_SANDBOX_BWRAP") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "AIKE_SANDBOX_BWRAP points at {}, which is not a file",
            p.display()
        ));
    }
    let path = std::env::var_os("PATH").ok_or_else(|| {
        "`sandbox` needs the bubblewrap binary, which is not installed, and PATH is \
         unset so it cannot be found. Install it: apt install bubblewrap / \
         dnf install bubblewrap / pacman -S bubblewrap / apk add bubblewrap."
            .to_owned()
    })?;
    std::env::split_paths(&path)
        .map(|d| d.join("bwrap"))
        .find(|c| c.is_file())
        .ok_or_else(|| {
            "`sandbox` needs the bubblewrap binary, which is not installed. \
             Install it: apt install bubblewrap / dnf install bubblewrap / \
             pacman -S bubblewrap / apk add bubblewrap."
                .to_owned()
        })
}

fn build_args(policy: &Policy, program: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut a: Vec<String> = vec![
        "--unshare-all".into(),
        "--die-with-parent".into(),
        // The read floor: the whole machine, read-only. This is the same
        // default the macOS backend gets from `(allow file-read*)` and the one
        // Claude Code and Codex use — reads are open, and `denied` is what
        // closes the credential files. Everything below overlays this base, so
        // it must come first.
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        // A fresh procfs, so the command sees only its own namespace's
        // processes instead of every process on the host.
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        // The host's /tmp is not what the command should see. This must come
        // before the cwd and grant binds below: a mount over /tmp covers
        // everything already mounted under it, so emitting it afterwards would
        // hide any policy path that lives in /tmp.
        "--tmpfs".into(),
        "/tmp".into(),
    ];

    // The working directory is the one writable place.
    a.push("--bind".into());
    a.push(s(&policy.working_dir));
    a.push(s(&policy.working_dir));

    for g in &policy.grants {
        // ⚠️ See the module docs: `--bind` and `--ro-bind` are the only two
        // options here, so `rw` also grants exec on Linux. A read-only grant
        // still needs its own mount: it may live under /tmp, which the tmpfs
        // above has already emptied.
        let flag = if g.access.can_write() {
            "--bind"
        } else {
            "--ro-bind-try"
        };
        a.push(flag.into());
        a.push(s(&g.path));
        a.push(s(&g.path));
    }

    // `denied` beats the read-all floor, so it is shadowed wherever it lives —
    // not only inside the writable cwd. A directory takes a private tmpfs and
    // its contents go away (the name stays); a file cannot take a tmpfs, so it
    // is bound over with /dev/null — name visible, reads fail. Both match what
    // the macOS backend gives with `(deny file-read-data)`.
    for d in &policy.denied {
        if d.is_dir() {
            a.push("--tmpfs".into());
            a.push(s(d));
        } else {
            a.push("--ro-bind".into());
            a.push("/dev/null".into());
            a.push(s(d));
        }
    }

    match &policy.network {
        // A network namespace with no interfaces: nothing can reach out, and
        // nothing can be reached. Already implied by --unshare-all; named here
        // so the intent is explicit and survives a change to the base flags.
        NetworkPolicy::DenyAll => a.push("--unshare-net".into()),
        // --unshare-all unshared every namespace, network included, so keeping
        // the host's network takes an explicit re-share. Without it `network:
        // "host"` silently had no route at all.
        NetworkPolicy::AllowAll => a.push("--share-net".into()),
    }

    a.push("--".into());
    a.push(s(program));
    a.extend(args.iter().map(|s| (*s).to_owned()));
    Ok(a)
}

/// The environment the command runs with.
///
/// `HOME` and `TMPDIR` point at the working directory, because it is the one
/// place the policy makes writable and the host's `/tmp` is a tmpfs the
/// namespace created empty. With a domain allowlist the host's proxy is
/// reachable only if the namespace is not unshared, so those policies get the
/// loopback the proxy listens on.
fn child_env(policy: &Policy, extra: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(path) = std::env::var_os("PATH") {
        env.insert("PATH".to_owned(), path.to_string_lossy().into_owned());
    }
    let cwd = policy.working_dir.to_string_lossy().into_owned();
    env.insert("HOME".to_owned(), cwd.clone());
    env.insert("TMPDIR".to_owned(), cwd);
    env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

fn into_file(pipe: impl IntoRawFd) -> std::fs::File {
    // SAFETY: the pipe came from a `Child` we own and have just taken.
    unsafe { std::fs::File::from_raw_fd(pipe.into_raw_fd()) }
}

#[cfg(test)]
mod tests {
    use super::super::path::testdir::TempDir;
    use super::super::policy::{Access, Grant};
    use super::*;

    fn policy() -> Policy {
        Policy {
            working_dir: PathBuf::from("/work"),
            grants: vec![Grant {
                path: PathBuf::from("/opt/tools"),
                access: Access::EXEC,
                is_dir: true,
            }],
            denied: vec![PathBuf::from("/work/.env")],
            network: NetworkPolicy::DenyAll,
        }
    }

    fn args() -> Vec<String> {
        build_args(&policy(), Path::new("/bin/ls"), &["-la"]).unwrap()
    }

    /// The value following the first occurrence of `flag`.
    fn value_after<'a>(a: &'a [String], flag: &str) -> Option<&'a str> {
        let at = a.iter().position(|x| x == flag)?;
        a.get(at + 1).map(String::as_str)
    }

    /// Every `(flag, target)` pair the builder emitted.
    fn mounts(a: &[String]) -> Vec<(&str, &str)> {
        a.iter()
            .enumerate()
            .filter(|(_, x)| {
                x.starts_with("--")
                    && matches!(x.as_str(), "--bind" | "--ro-bind" | "--ro-bind-try")
            })
            .filter_map(|(i, _)| a.get(i + 1).map(|t| (a[i].as_str(), t.as_str())))
            .collect()
    }

    #[test]
    fn a_denied_network_gets_its_own_namespace() {
        assert!(args().contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn host_network_re_shares_the_namespace_unshare_all_took_away() {
        // --unshare-all includes the network namespace, so "host" needs an
        // explicit --share-net; emitting nothing left the sandbox with only lo.
        let a = build_args(
            &Policy {
                network: NetworkPolicy::AllowAll,
                ..policy()
            },
            Path::new("/bin/ls"),
            &[],
        )
        .unwrap();
        assert!(a.contains(&"--share-net".to_string()), "{a:?}");
        assert!(!a.contains(&"--unshare-net".to_string()), "{a:?}");
    }

    #[test]
    fn the_command_runs_after_a_double_dash() {
        // Without `--`, a command whose name starts with `-` would be read as
        // an option to bwrap itself.
        let a = args();
        let at = a.iter().position(|x| x == "--").expect("separator");
        assert_eq!(a[at + 1], "/bin/ls");
        assert_eq!(a[at + 2], "-la");
    }

    #[test]
    fn the_working_directory_is_bound_writable() {
        let a = args();
        let m = mounts(&a);
        assert!(
            m.contains(&("--bind", "/work")),
            "the cwd must be bound read-write: {m:?}"
        );
    }

    /// Every value a `--tmpfs` was given, in order.
    fn tmpfs_targets(a: &[String]) -> Vec<&str> {
        a.iter()
            .enumerate()
            .filter(|(_, x)| x.as_str() == "--tmpfs")
            .filter_map(|(i, _)| a.get(i + 1).map(String::as_str))
            .collect()
    }

    #[test]
    fn a_denied_file_inside_the_working_directory_is_hidden_with_dev_null() {
        // bwrap cannot put a tmpfs on a file, so a denied file is bound over
        // with /dev/null: the name stays visible and reads fail — the shape the
        // macOS backend gives with (deny file-read-data). `args()` denies a
        // non-directory path, which is the file branch.
        let a = args();
        assert!(
            a.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == "/work/.env"),
            "{a:?}"
        );
        assert!(
            !tmpfs_targets(&a).contains(&"/work/.env"),
            "a file must not get a tmpfs: {a:?}"
        );
    }

    #[test]
    fn a_denied_directory_inside_the_working_directory_is_shadowed() {
        // A directory can take a tmpfs, so it disappears entirely.
        let dir = TempDir::new();
        let cwd = dir.path().join("work");
        let secret = cwd.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        let p = Policy {
            working_dir: cwd,
            grants: vec![],
            denied: vec![secret.clone()],
            network: NetworkPolicy::DenyAll,
        };
        let a = build_args(&p, Path::new("/bin/ls"), &[]).unwrap();
        let target = secret.to_string_lossy();
        assert!(
            tmpfs_targets(&a).contains(&target.as_ref()),
            "the denied directory must be shadowed: {a:?}"
        );
    }

    #[test]
    fn the_private_tmp_does_not_shadow_the_working_directory() {
        // A mount over /tmp covers everything already mounted under it, so the
        // private /tmp has to be emitted before the cwd (and grant) binds; the
        // other order silently hides any policy path that lives in /tmp.
        let a = args();
        let tmp = a.iter().position(|x| x == "--tmpfs").unwrap();
        let cwd = a.iter().position(|x| x == "--bind").unwrap();
        assert_eq!(a[tmp + 1], "/tmp", "{a:?}");
        assert!(tmp < cwd, "--tmpfs /tmp must precede the cwd bind: {a:?}");
    }

    #[test]
    fn proc_and_dev_are_fresh_rather_than_the_hosts() {
        let a = args();
        assert_eq!(value_after(&a, "--proc"), Some("/proc"), "{a:?}");
        assert_eq!(value_after(&a, "--dev"), Some("/dev"), "{a:?}");
    }

    #[test]
    fn the_whole_machine_is_readable_read_only() {
        // The read floor: everything is readable, matching macOS's
        // (allow file-read*) and the mainstream agents' default. Without this
        // the sandbox could only read the directories it happened to bind.
        let a = args();
        let m = mounts(&a);
        assert!(m.contains(&("--ro-bind", "/")), "{m:?}");
    }

    #[test]
    fn a_granted_path_is_bound_at_the_same_place_it_lives() {
        let a = args();
        let m = mounts(&a);
        assert!(m.contains(&("--ro-bind-try", "/opt/tools")), "{m:?}");
    }
}
