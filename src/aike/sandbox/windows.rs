// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Windows backend: an AppContainer whose package SID is granted per-path ACL
//! entries.
//!
//! An AppContainer token is default-deny against the filesystem: it reaches
//! only paths whose ACL names its package SID. That is the same shape the other
//! two backends express, so a configured path is granted one entry at a time
//! and nothing else is reachable.
//!
//! Directory grants need two ACEs rather than one. On NTFS, "run a file" and
//! "enter a directory" are the same access bit (`FILE_EXECUTE` /
//! `FILE_TRAVERSE`, `0x20`), so a single inherited ACE would make a writable
//! directory's files executable too. The workaround is inheritance direction:
//!
//! - one ACE with `OBJECT_INHERIT_ACE` carrying read (+write) (+execute), which
//!   reaches files but not subdirectories;
//! - one ACE with `CONTAINER_INHERIT_ACE` carrying read + traverse (+write),
//!   which reaches subdirectories but not files.
//!
//! That is what makes "writable but not runnable" expressible here, and it is
//! the one place this backend is more involved than macOS's SBPL text.
//!
//! `rappct`'s `grant_to_package` cannot express that split, so directory ACLs
//! are written here directly through `windows-sys`; everything else (profile,
//! capabilities, container launch, stdio) goes through `rappct`.
//!
//! It has to be in-process. The capability set is attached to the child at
//! creation time through `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, so
//! there is no "exec.exe" equivalent to shell out to — a host that wanted one
//! would have to write a relay that proxies stdio and reaps the child.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString, c_void},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use rappct::{
    AppContainerProfile, KnownCapability, LaunchOptions, SecurityCapabilitiesBuilder, StdioConfig,
    launch::{JobLimits, launch_in_container_with_io},
};

use super::{
    child::{ChildControl, SpawnedChild, WaitResult},
    policy::{Access, NetworkPolicy, Policy},
};

/// Read the data/metadata of a file, or list a directory. Includes the
/// synchronize and read-control bits a normal handle open expects.
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
/// Same bit as `FILE_EXECUTE`; named separately because for a directory it
/// means "walk through" rather than "run".
const FILE_TRAVERSE: u32 = 0x20;

/// `OBJECT_INHERIT_ACE` / `CONTAINER_INHERIT_ACE`.
const OBJECT_INHERIT: u32 = 0x1;
const CONTAINER_INHERIT: u32 = 0x2;

pub(crate) fn spawn(
    policy: &Policy,
    program: &Path,
    args: &[&str],
    env: &HashMap<String, String>,
) -> Result<SpawnedChild, String> {
    let name = unique_profile_name("aike-sbx");
    let profile = AppContainerProfile::ensure(&name, "aike sandbox", Some("aike sandbox"))
        .map_err(|e| format!("`sandbox` cannot create an AppContainer profile: {e}"))?;
    let sid = profile.sid.as_string().to_owned();

    // Every path we touch is recorded so its ACEs can be revoked when the run
    // ends; a DACL is persistent, so otherwise every spawn would leave another
    // entry on the host's filesystem.
    let mut granted: Vec<PathBuf> = Vec::new();

    // The working directory is read-write, never executable.
    if let Err(e) = grant(&policy.working_dir, true, Access::WRITE, &sid) {
        delete_profile(&name);
        return Err(e);
    }
    granted.push(policy.working_dir.clone());

    for g in &policy.grants {
        let result = if g.is_dir {
            grant(&g.path, true, g.access, &sid)
        } else {
            grant(&g.path, false, g.access, &sid)
        };
        if let Err(e) = result {
            revoke_all(&granted, &sid);
            delete_profile(&name);
            return Err(e);
        }
        granted.push(g.path.clone());
        if !g.is_dir
            && let Some(parent) = g.path.parent()
        {
            // A file grant needs its parent directory reachable too: opening
            // `<dir>\file` requires read/traverse on `<dir>`, and an
            // AppContainer has no bypass-traverse privilege to skip it. The
            // ACE is non-inherited so siblings stay private.
            if let Err(e) = apply_entries(parent, &sid, &[(FILE_GENERIC_READ | FILE_TRAVERSE, 0)]) {
                revoke_all(&granted, &sid);
                delete_profile(&name);
                return Err(e);
            }
            granted.push(parent.to_path_buf());
        }
    }

    if let Err(e) = entry_is_executable(program, policy) {
        revoke_all(&granted, &sid);
        delete_profile(&name);
        return Err(e);
    }

    let capabilities = match build_capabilities(policy, &profile.sid) {
        Ok(c) => c,
        Err(e) => {
            revoke_all(&granted, &sid);
            delete_profile(&name);
            return Err(e);
        }
    };
    let options = LaunchOptions {
        exe: strip_verbatim(program),
        cmdline: Some(command_line(program, args)),
        cwd: Some(strip_verbatim(&policy.working_dir)),
        env: Some(child_env(policy, env)),
        stdio: StdioConfig::Pipe,
        suspended: false,
        // kill_on_job_close is the guarantee that a timeout (or aike dying)
        // takes the whole tree down, not just the process we started.
        join_job: Some(JobLimits {
            memory_bytes: None,
            cpu_rate_percent: None,
            kill_on_job_close: true,
        }),
        startup_timeout: None,
    };
    let mut launched = match launch_in_container_with_io(&capabilities, &options) {
        Ok(l) => l,
        Err(e) => {
            revoke_all(&granted, &sid);
            delete_profile(&name);
            return Err(format!("`sandbox` cannot start the child: {e}"));
        }
    };

    let pid = launched.pid as i32;
    let job = match launched.job_guard.as_ref() {
        Some(guard) => match JobHandle::duplicate(guard.as_handle().0 as *mut c_void) {
            Ok(j) => j,
            Err(e) => {
                revoke_all(&granted, &sid);
                delete_profile(&name);
                return Err(e);
            }
        },
        None => {
            revoke_all(&granted, &sid);
            delete_profile(&name);
            return Err("`sandbox` started the child without a job object".to_owned());
        }
    };

    let control = ChildControl::new(
        {
            let job = Arc::clone(&job);
            move || job.terminate()
        },
        move || is_running(pid),
        // Windows has no signals to deliver, so there is nothing to report as
        // delivered. The tree-killing `kill` above is the only primitive.
        move |_sig| false,
    );

    // The pipes leave `launched` before it is moved into the reaper.
    let child_stdin = launched.stdin.take();
    let child_stdout = launched.stdout.take();
    let child_stderr = launched.stderr.take();

    Ok(SpawnedChild::new(
        pid,
        child_stdin,
        child_stdout,
        child_stderr,
        control,
        move || {
            let code = launched
                .wait(None)
                .map_err(|e| format!("`sandbox` cannot wait for the child: {e}"))?;
            revoke_all(&granted, &sid);
            delete_profile(&name);
            Ok(WaitResult {
                exit_code: code as i32,
                // Windows has no signal concept; termination is always an exit
                // code.
                signal: None,
                killed_by_timeout: false,
            })
        },
    ))
}

/// Whether an AppContainer profile can be created without elevation.
///
/// The profile is per-user, so this is expected to succeed as an ordinary user;
/// the probe exists to turn "we assume so" into "this machine agrees".
pub(crate) fn probe_profile() -> Result<(), String> {
    let name = unique_profile_name("aike-probe");
    AppContainerProfile::ensure(&name, "aike", None).map_err(|e| e.to_string())?;
    delete_profile(&name);
    Ok(())
}

/// `DeleteAppContainerProfile` lives in `Userenv.dll`; `rappct` only exposes it
/// as a consuming method, which does not fit the several exit paths here.
fn delete_profile(name: &str) {
    #[link(name = "Userenv")]
    unsafe extern "system" {
        fn DeleteAppContainerProfile(name: *const u16) -> i32;
    }
    let wide: Vec<u16> = OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string for the duration of the
    // call; the return value is not needed for cleanup.
    unsafe {
        DeleteAppContainerProfile(wide.as_ptr());
    }
}

// ---------------------------------------------------------------------------
// entry-point check
// ---------------------------------------------------------------------------

/// Refuse to *start* a program the sandbox itself made writable-but-not-
/// executable.
///
/// On Windows the process that calls `CreateProcess` maps the entry image, so
/// the image's own ACL never gates the first `exec`: an image written into the
/// working directory still launches. In-container access is still ACL-checked,
/// so a program started from outside the writable area cannot in turn launch a
/// payload that the container lacks execute on; this closes the direct case.
///
/// A program outside the writable area (a system binary, `cmd.exe`, the
/// interpreter the caller chose) is left alone.
fn entry_is_executable(program: &Path, policy: &Policy) -> Result<(), String> {
    if super::path::is_within(program, &policy.working_dir) {
        return Err(format!(
            "`sandbox` refuses to run `{}`: it is inside the working directory, which is \
             writable but not executable. Grant it `rx` (or `rwx`) to run programs there.",
            program.display()
        ));
    }
    let mut covered = false;
    for g in &policy.grants {
        if super::path::is_within(program, &g.path) {
            covered = true;
            if g.access.can_execute() {
                return Ok(());
            }
        }
    }
    if covered {
        return Err(format!(
            "`sandbox` refuses to run `{}`: its directory is granted without execute. Pass \
             `rx` or `rwx` to allow running programs from it.",
            program.display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ACL grants
// ---------------------------------------------------------------------------

/// Grant `sid` access to `path`. A directory gets the two-ACE tree described in
/// the module docs; a file gets one non-inherited ACE, since there is nothing
/// beneath it to separate.
fn grant(path: &Path, is_dir: bool, access: Access, sid: &str) -> Result<(), String> {
    let mut file_mask = FILE_GENERIC_READ;
    let mut dir_mask = FILE_GENERIC_READ | FILE_TRAVERSE;
    if access.can_write() {
        file_mask |= FILE_GENERIC_WRITE;
        dir_mask |= FILE_GENERIC_WRITE;
    }
    if access.can_execute() {
        file_mask |= FILE_GENERIC_EXECUTE;
    }

    if is_dir {
        apply_entries(
            path,
            sid,
            &[(dir_mask, CONTAINER_INHERIT), (file_mask, OBJECT_INHERIT)],
        )
    } else {
        apply_entries(path, sid, &[(file_mask, 0)])
    }
}

/// Remove every ACE naming `sid` from `path`, at all three inheritance levels
/// a grant can produce.
fn revoke(path: &Path, sid: &str) {
    // Failure is not fatal: the run already finished, so a leftover ACE is a
    // cleanup miss rather than a containment failure.
    let _ = apply_entries(
        path,
        sid,
        &[(0, 0), (0, OBJECT_INHERIT), (0, CONTAINER_INHERIT)],
    );
}

fn revoke_all(paths: &[PathBuf], sid: &str) {
    for p in paths {
        revoke(p, sid);
    }
}

/// The one place `SetEntriesInAclW` is called. Each `(mask, inheritance)` pair
/// becomes one `EXPLICIT_ACCESS_W`; a zero mask selects `REVOKE_ACCESS`.
fn apply_entries(path: &Path, sid: &str, entries: &[(u32, u32)]) -> Result<(), String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::{
        ACL,
        Authorization::{
            ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
            REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
            TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
        },
        DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let sid_w: Vec<u16> = sid.encode_utf16().chain(std::iter::once(0)).collect();
    let count = entries.len() as u32;

    // SAFETY: every pointer below is either a locally owned allocation freed
    // on the way out, or comes from a Win32 call whose contract is satisfied.
    // `psid`, `old_sd` and `new_dacl` are each freed exactly once.
    unsafe {
        let mut psid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(sid_w.as_ptr(), &mut psid) == 0 {
            return Err("`sandbox` could not convert the AppContainer SID".to_owned());
        }

        let mut trustee: TRUSTEE_W = std::mem::zeroed();
        trustee.TrusteeForm = TRUSTEE_IS_SID;
        trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
        trustee.ptstrName = psid as *mut u16;

        let explicit: Vec<EXPLICIT_ACCESS_W> = entries
            .iter()
            .map(|(mask, inheritance)| {
                let mut ea: EXPLICIT_ACCESS_W = std::mem::zeroed();
                ea.grfAccessPermissions = *mask;
                ea.grfAccessMode = if *mask == 0 {
                    REVOKE_ACCESS
                } else {
                    GRANT_ACCESS
                };
                ea.grfInheritance = *inheritance;
                ea.Trustee = trustee;
                ea
            })
            .collect();

        let mut old_sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut old_dacl: *mut ACL = std::ptr::null_mut();
        let st = GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut old_sd,
        );
        if st != 0 {
            LocalFree(psid);
            return Err(format!(
                "`sandbox` cannot read the ACL of `{}`: Win32 error {st}",
                path.display()
            ));
        }

        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let st = SetEntriesInAclW(count, explicit.as_ptr(), old_dacl, &mut new_dacl);
        if st != 0 {
            LocalFree(psid);
            LocalFree(old_sd);
            return Err(format!(
                "`sandbox` cannot build an ACL for `{}`: Win32 error {st}",
                path.display()
            ));
        }

        let st = SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        LocalFree(psid);
        LocalFree(old_sd);
        LocalFree(new_dacl as *mut c_void);
        if st != 0 {
            return Err(format!(
                "`sandbox` cannot set the ACL of `{}`: Win32 error {st}",
                path.display()
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// capabilities, environment and command line
// ---------------------------------------------------------------------------

fn build_capabilities(
    policy: &Policy,
    sid: &rappct::AppContainerSid,
) -> Result<rappct::SecurityCapabilities, String> {
    let builder = SecurityCapabilitiesBuilder::new(sid);
    let builder = match policy.network {
        // No capability at all: the container has no outbound route. This is
        // the complete denial, not a partial one.
        NetworkPolicy::DenyAll => builder,
        NetworkPolicy::AllowAll => builder.with_known(&[KnownCapability::InternetClient]),
    };
    builder
        .build()
        .map_err(|e| format!("`sandbox` cannot build the capability set: {e}"))
}

/// The environment the child runs with.
///
/// The host's environment is not inherited wholesale — it carries whatever
/// secrets the user's shell holds — but Windows programs need a handful of
/// machine variables to start at all (`SystemRoot` above everything), which
/// `merge_parent_env` adds back. `LOCALAPPDATA` is added explicitly: without it
/// the AppContainer launch itself fails with `ERROR_ENVVAR_NOT_FOUND` (203),
/// because the runtime resolves the container's per-package folder through it.
/// `TEMP`/`TMP` point at the working directory, the one place the policy makes
/// writable, rather than the host's temp dir.
fn child_env(policy: &Policy, extra: &HashMap<String, String>) -> Vec<(OsString, OsString)> {
    let cwd = strip_verbatim(&policy.working_dir).into_os_string();
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    // Machine/user variables the loader and the container need. None of these
    // is a secret; the caller's own environment is still not inherited.
    for key in ["PATH", "LOCALAPPDATA", "APPDATA", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            env.push((OsString::from(key), value));
        }
    }
    for key in ["TEMP", "TMP", "TMPDIR", "HOME"] {
        env.push((OsString::from(key), cwd.clone()));
    }
    for (k, v) in extra {
        env.push((OsString::from(k), OsString::from(v)));
    }
    rappct::launch::merge_parent_env(env)
}

/// The `lpCommandLine` handed to `CreateProcessW`: the program, then each
/// argument, quoted by the same rules `std::process::Command` uses. `rappct`
/// passes this through untranslated, so getting it wrong is how a path or an
/// argument would become a second command.
fn command_line(program: &Path, args: &[&str]) -> String {
    let mut line = String::new();
    append_quoted(&mut line, &program.to_string_lossy());
    for arg in args {
        line.push(' ');
        append_quoted(&mut line, arg);
    }
    line
}

/// Quote one argument for a Windows command line, matching what
/// `std::process::Command` emits (the MSVCRT "Parsing C++ Command-Line
/// Arguments" rules): quote when the argument is empty or contains whitespace,
/// and double the backslashes that precede a quote.
fn append_quoted(out: &mut String, arg: &str) {
    let quote = arg.is_empty() || arg.contains([' ', '\t']);
    if quote {
        out.push('"');
    }
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            out.push('\\');
        } else {
            if c == '"' {
                // The backslashes directly before a quote must be doubled so
                // the quote stays escaped.
                out.push_str(&"\\".repeat(backslashes + 1));
            }
            backslashes = 0;
            out.push(c);
        }
    }
    if quote {
        // Trailing backslashes would otherwise escape the closing quote.
        out.push_str(&"\\".repeat(backslashes));
        out.push('"');
    }
}

/// Drop the `\\?\` prefix `canonicalize` adds.
///
/// `CreateProcessW` does not accept the verbatim form for `lpCurrentDirectory`
/// (the child silently starts in the wrong place), while the ACL and launch
/// APIs are just as happy with the plain absolute path. `\\?\UNC\server\share`
/// becomes `\\server\share`, the form Win32 expects.
fn strip_verbatim(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// A profile name no concurrent sandbox shares: profiles are keyed by name, so
/// a shared one would mean shared SIDs and shared ACL entries.
fn unique_profile_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // AppContainer profile names are limited to 64 characters.
    format!("{prefix}-{now:016x}{:08x}", seq & 0xffff_ffff)
}

// ---------------------------------------------------------------------------
// job handle + liveness
// ---------------------------------------------------------------------------

/// An owned job-object handle. `TerminateJobObject` on it takes down every
/// process in the job, which is how the tree is killed.
struct JobHandle(*mut c_void);

// SAFETY: a kernel handle is an index into the process handle table and may be
// used from any thread; the Win32 calls below are thread-safe.
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl JobHandle {
    /// Duplicate the job handle `rappct` created, so it outlives the `JobGuard`
    /// that owns the original.
    fn duplicate(source: *mut c_void) -> Result<Arc<Self>, String> {
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let mut dup: *mut c_void = std::ptr::null_mut();
        // SAFETY: both process handles are the current-process pseudo-handle,
        // and `dup` is a valid out-parameter.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source,
                GetCurrentProcess(),
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err("`sandbox` could not duplicate the job handle".to_owned());
        }
        Ok(Arc::new(JobHandle(dup)))
    }

    fn terminate(&self) -> bool {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: the handle is owned and valid for the lifetime of `self`.
        unsafe { TerminateJobObject(self.0, 1) != 0 }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: we own the handle and close it exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn is_running(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // SAFETY: OpenProcess returns a fresh handle or null; both are handled.
    // SYNCHRONIZE is required for WaitForSingleObject — query rights alone do
    // not grant it, and without it the wait fails rather than reporting
    // "still running".
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            pid as u32,
        );
        if handle.is_null() {
            return false;
        }
        let status = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        status == WAIT_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arguments_are_not_quoted() {
        let mut s = String::new();
        append_quoted(&mut s, "plain");
        assert_eq!(s, "plain");
    }

    #[test]
    fn an_argument_with_a_space_is_quoted() {
        let mut s = String::new();
        append_quoted(&mut s, "two words");
        assert_eq!(s, "\"two words\"");
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        // An argument without whitespace is not wrapped, but an internal quote
        // is still escaped.
        let mut s = String::new();
        append_quoted(&mut s, r#"a"b"#);
        assert_eq!(s, r#"a\"b"#);

        // Backslashes not before a quote stay literal.
        let mut s = String::new();
        append_quoted(&mut s, r"a\\b");
        assert_eq!(s, r"a\\b");

        // Backslash before a quote: doubled, then the quote escaped.
        let mut s = String::new();
        append_quoted(&mut s, r#"\""#);
        assert_eq!(s, r#"\\\""#);

        // Trailing backslashes must not escape the closing quote.
        let mut s = String::new();
        append_quoted(&mut s, r"trail \");
        assert_eq!(s, r#""trail \\""#);
    }

    #[test]
    fn an_empty_argument_becomes_a_quoted_pair() {
        let mut s = String::new();
        append_quoted(&mut s, "");
        assert_eq!(s, "\"\"");
    }

    #[test]
    fn a_command_line_starts_with_the_program() {
        let line = command_line(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            &["/c", "echo hi"],
        );
        assert_eq!(line, r#"C:\Windows\System32\cmd.exe /c "echo hi""#);
    }

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\tmp\x")),
            PathBuf::from(r"C:\tmp\x")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\x")),
            PathBuf::from(r"\\server\share\x")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"C:\tmp\x")),
            PathBuf::from(r"C:\tmp\x")
        );
    }

    #[test]
    fn a_writable_directory_does_not_grant_execute_to_its_files() {
        // The two-ACE split is the whole reason this backend has an ACL layer
        // of its own: `rw` must let a file be written but not run.
        let mut file_mask = FILE_GENERIC_READ;
        let mut dir_mask = FILE_GENERIC_READ | FILE_TRAVERSE;
        let access = Access::parse("rw").unwrap();
        if access.can_write() {
            file_mask |= FILE_GENERIC_WRITE;
            dir_mask |= FILE_GENERIC_WRITE;
        }
        if access.can_execute() {
            file_mask |= FILE_GENERIC_EXECUTE;
        }
        assert_eq!(dir_mask & FILE_TRAVERSE, FILE_TRAVERSE);
        // FILE_EXECUTE is the same bit as FILE_TRAVERSE; assert the file mask
        // does not carry it.
        assert_eq!(file_mask & FILE_TRAVERSE, 0, "{file_mask:#x}");
    }
}
