// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Chrome process management — find, spawn, stop.

use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{process::Child, time::sleep};

/// Per-launch sequence number, makes auto-port user-data-dirs unique.
static LAUNCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Child handles of live `Browser.spawn()` processes, killed at runtime exit.
/// Dropping / GC-collecting the JS handle does NOT kill the process — that is
/// the whole point of the explicit-ownership model (`kill()` or exit cleanup).
static SPAWNED_BROWSERS: OnceLock<Mutex<Vec<Arc<tokio::sync::Mutex<Option<Child>>>>>> =
    OnceLock::new();

/// Kill every registered `Browser.spawn()` child. Called from the engine's
/// atexit / signal-handler cleanup alongside `spawn::kill_all_children`.
pub(crate) fn kill_all_spawned_browsers() {
    if let Some(reg) = SPAWNED_BROWSERS.get()
        && let Ok(guard) = reg.lock()
    {
        for arc in guard.iter() {
            if let Ok(mut slot) = arc.try_lock()
                && let Some(child) = slot.as_mut()
            {
                let _ = child.start_kill();
            }
        }
    }
}

pub(crate) fn register_spawned_child(handle: &Arc<tokio::sync::Mutex<Option<Child>>>) {
    let reg = SPAWNED_BROWSERS.get_or_init(|| Mutex::new(Vec::new()));
    reg.lock().unwrap().push(handle.clone());
}

/// A spawned browser CDP server: resolved binary, process handle, and the
/// ready browser-level CDP WebSocket URL.
pub(crate) struct SpawnedBrowser {
    pub(crate) bin: String,
    pub(crate) child: Child,
    pub(crate) cdp_url: String,
}

/// Spawn a browser CDP server process and wait until its endpoint is ready.
///
/// - `args == None` → auto mode: assemble the standard Chrome command line
///   (`headless`/`proxy_url` apply); `bin` defaults to auto-detected Chrome.
///   `port == 0` picks a free port discovered via DevToolsActivePort.
/// - `args == Some` → custom command line (e.g. lightpanda): `bin` required,
///   `port` must be explicit and the endpoint is polled at that port.
pub(crate) async fn spawn(
    bin: Option<&str>,
    args: Option<&[String]>,
    headless: bool,
    port: u16,
    proxy_url: Option<&str>,
    kill_on_drop: bool,
) -> Result<SpawnedBrowser, String> {
    let user_data_dir = auto_data_dir(port);
    let (bin, args) = match (bin, args) {
        (Some(bin), Some(args)) => {
            if port == 0 {
                return Err("`args` requires an explicit `port`".to_string());
            }
            (bin.to_string(), args.to_vec())
        }
        (bin, None) => {
            let bin = bin
                .map(|s| s.to_string())
                .or_else(find_chrome)
                .ok_or_else(|| {
                    "Chrome/Chromium not found. Install Chrome or set `bin`.".to_string()
                })?;
            (bin, chrome_args(headless, port, proxy_url, &user_data_dir))
        }
        (None, Some(_)) => return Err("`args` requires `bin`".to_string()),
    };

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if kill_on_drop {
        cmd.kill_on_drop(true);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{bin}`: {e}"))?;

    // Auto port: discover the real endpoint from DevToolsActivePort; explicit
    // port: poll the known /json/version endpoint.
    let cdp_url = if port == 0 {
        wait_for_devtools_port(&user_data_dir).await?
    } else {
        wait_for_debug_url(port).await?
    };

    Ok(SpawnedBrowser {
        bin,
        child,
        cdp_url,
    })
}

/// `Browser.launch()` sugar: auto-mode spawn owning the process (killed when
/// the owning Browser object is dropped / closed).
pub(crate) async fn launch(
    headless: bool,
    port: u16,
    proxy_url: Option<&str>,
    chrome_path: Option<&str>,
) -> Result<(Child, String), String> {
    let spawned = spawn(chrome_path, None, headless, port, proxy_url, true).await?;
    Ok((spawned.child, spawned.cdp_url))
}

/// Unique per-launch user-data-dir (pid + seq); explicit-port launches keep
/// the historical `aike-chrome-<port>` name.
fn auto_data_dir(port: u16) -> std::path::PathBuf {
    let dir_name = if port == 0 {
        format!(
            "aike-chrome-{}-{}",
            std::process::id(),
            LAUNCH_SEQ.fetch_add(1, Ordering::Relaxed)
        )
    } else {
        format!("aike-chrome-{}", port)
    };
    std::env::temp_dir().join(dir_name)
}

/// Assemble the standard Chrome command line (incl. anti-detection flags).
fn chrome_args(
    headless: bool,
    port: u16,
    proxy_url: Option<&str>,
    user_data_dir: &std::path::Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        format!("--remote-debugging-port={}", port),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-sync".to_string(),
        "--disable-background-networking".to_string(),
        "--window-size=1280,800".to_string(),
        // Use a temp user-data-dir so remote debugging port reliably opens.
        // Without this, modern Chrome on macOS may silently skip the debug.
        // Set window-size so the actual Chrome frame is large enough.
        format!("--user-data-dir={}", user_data_dir.display()),

        // === Anti-detection parameters (Phase 1) ===
        // Hide navigator.webdriver attribute (most important one)
        "--disable-blink-features=AutomationControlled".to_string(),
        // Disable automation infobar
        "--disable-automation".to_string(),
        // Disable various background features to reduce fingerprint
        "--disable-component-update".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-notifications".to_string(),
        "--disable-popup-blocking".to_string(),
        "--disable-extensions".to_string(),
        "--disable-domain-reliability".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-backgrounding-occluded-windows".to_string(),
        "--disable-features=ChromeWhatsNewUI,InterestFeedLoading,MediaRouter,NavigationThreading,OptimizationHints".to_string(),
        "--disable-features=SafeBrowsingEnhancedProtection".to_string(),
        "--disable-webrtc".to_string(),
        "--lang=en-US".to_string(),
        "--disable-dev-shm-usage".to_string(),
    ];

    if headless {
        args.push("--headless=new".to_string());
        // Only disable sandbox in headless mode (security risk otherwise)
        args.push("--no-sandbox".to_string());
        // Reduce crashes in some headless environments
        args.push("--disable-gpu".to_string());
    }
    if let Some(proxy) = proxy_url {
        args.push(format!("--proxy-server={}", proxy));
    }
    args
}

/// Poll `<user-data-dir>/DevToolsActivePort` (written by Chrome once the debug
/// server binds) and build the browser WS url from it: line 1 is the port,
/// line 2 is the browser target path.
async fn wait_for_devtools_port(user_data_dir: &std::path::Path) -> Result<String, String> {
    let file = user_data_dir.join("DevToolsActivePort");
    for _ in 0..300 {
        if let Ok(content) = std::fs::read_to_string(&file) {
            let mut lines = content.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next())
                && let Ok(port) = port.trim().parse::<u16>()
            {
                return Ok(format!("ws://127.0.0.1:{}{}", port, path.trim()));
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err("Chrome did not write DevToolsActivePort within 30 seconds".into())
}

/// Poll Chrome's /json/version until webSocketDebuggerUrl appears.
async fn wait_for_debug_url(port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{}/json/version", port);
    let client = reqwest::Client::new();

    for _ in 0..60 {
        if let Ok(resp) = client
            .get(&url)
            .timeout(Duration::from_secs(1))
            .send()
            .await
            && let Ok(json) = resp.json::<serde_json::Value>().await
            && let Some(ws_url) = json.get("webSocketDebuggerUrl").and_then(|v| v.as_str())
        {
            return Ok(ws_url.to_string());
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err("Chrome did not start within 30 seconds".into())
}

/// Find Chrome/Chromium executable path.
///
/// Search order: known per-platform install locations first, then the PATH via
/// the same resolver `Aike.which` uses (`which_executable`), so both the
/// bundled-paths and the command-name lookups share one logic.
fn find_chrome() -> Option<String> {
    // Known install locations that are not on PATH.
    #[cfg(target_os = "macos")]
    {
        let paths = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ];
        for p in &paths {
            if std::path::Path::new(p).exists() {
                return Some(p.to_string());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let paths = [
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
            "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
            "C:\\Program Files\\Chromium\\Application\\chrome.exe",
            "C:\\Program Files\\BraveSoftware\\Brave-Browser\\Application\\brave.exe",
        ];
        for p in &paths {
            if std::path::Path::new(p).exists() {
                return Some(p.to_string());
            }
        }
    }

    // Fall back to searching PATH for the common command names. Reuses the
    // crate-wide resolver (PATHEXT-aware on Windows, executable-bit check on
    // POSIX) instead of shelling out to `which`/`where`.
    const CANDIDATES: &[&str] = &[
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "chrome",
    ];
    CANDIDATES
        .iter()
        .find_map(|name| crate::aike::which_executable(name))
}
