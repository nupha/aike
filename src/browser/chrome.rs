// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Chrome process management — find, launch, stop.

use std::time::Duration;

use tokio::{process::Child, time::sleep};

/// Launch Chrome with remote debugging enabled.
/// Returns (child_handle, webSocketDebuggerUrl).
pub(crate) async fn launch(
    headless: bool,
    port: u16,
    proxy_url: Option<&str>,
    chrome_path: Option<&str>,
) -> Result<(Child, String), String> {
    let path = chrome_path
        .map(|p| p.to_string())
        .or_else(find_chrome)
        .ok_or_else(|| {
            "Chrome/Chromium not found. Install Chrome or set chromePath.".to_string()
        })?;

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
        format!(
            "--user-data-dir={}",
            std::env::temp_dir()
                .join(format!("aike-chrome-{}", port))
                .display()
        ),

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

    let child = tokio::process::Command::new(&path)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to launch Chrome: {e}"))?;

    // Wait for Chrome's DevTools HTTP endpoint to become ready
    let ws_url = wait_for_debug_url(port).await?;

    Ok((child, ws_url))
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
        {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(ws_url) = json.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) {
                    return Ok(ws_url.to_string());
                }
            }
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
