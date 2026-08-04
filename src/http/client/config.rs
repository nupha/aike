// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `HttpClientConfig` — client-level settings for the reusable `HttpClient`.
//!
//! Holds proxy, TLS, cookie, DNS, and connection-level options.
//! `build_http_client()` constructs a `reqwest::Client` from the config.

use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

/// Protocol version preference.
#[derive(Clone, Debug, Default)]
pub(crate) enum ProtocolMode {
    /// Let reqwest negotiate (H1/H2 via ALPN).
    #[default]
    Auto,
    /// Force HTTP/1.x only.
    Http1Only,
    /// Assume the server speaks HTTP/2 from the start (no upgrade).
    Http2PriorKnowledge,
}

/// Client-level configuration for `HttpClient`.
///
/// Set once in the constructor; all subsequent requests on the same
/// `HttpClient` instance share the same settings and connection pool.
#[derive(Clone, Debug)]
pub(crate) struct HttpClientConfig {
    /// HTTP/HTTPS proxy URL.
    pub proxy: Option<String>,
    /// Comma-separated domains to bypass the proxy.
    pub no_proxy: Option<String>,

    /// Skip TLS certificate validation (dangerous).
    pub danger_accept_invalid_certs: bool,
    /// PEM-encoded CA certificate to add to the trust root.
    pub root_cert_pem: Option<String>,
    /// PEM-encoded client certificate (for mTLS).
    pub client_cert_pem: Option<String>,
    /// PEM-encoded client private key (for mTLS).
    pub client_key_pem: Option<String>,

    /// Enable cookie jar for cross-request persistence.
    pub cookie_jar: bool,

    /// DNS overrides: host → IP.
    pub resolve_overrides: Vec<(String, SocketAddr)>,
    /// Protocol version preference.
    pub protocol: ProtocolMode,

    /// Connection timeout (seconds).
    pub connect_timeout: Duration,

    /// Let the client follow redirects itself. Turn this off when the caller
    /// follows hops manually (e.g. fetch, which must recompute `Referer` per
    /// hop) so redirect responses are surfaced instead of swallowed.
    pub follow_redirects: bool,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            no_proxy: None,
            danger_accept_invalid_certs: false,
            root_cert_pem: None,
            client_cert_pem: None,
            client_key_pem: None,
            cookie_jar: true,
            resolve_overrides: vec![],
            protocol: ProtocolMode::Auto,
            connect_timeout: Duration::from_secs(10),
            follow_redirects: true,
        }
    }
}

impl HttpClientConfig {
    /// Build a `reqwest::Client` from the configuration.
    #[allow(dead_code)]
    pub fn build_http_client(&self) -> Result<reqwest::Client, String> {
        self.build_http_client_ex(self.danger_accept_invalid_certs)
    }

    /// Build a `reqwest::Client`, optionally skipping cert validation.
    ///
    /// `insecure` overrides `self.danger_accept_invalid_certs`: when true the
    /// client accepts *any* certificate (test/dev escape hatch for self-signed
    /// hosts). Validation stays on for every other host.
    pub fn build_http_client_ex(&self, insecure: bool) -> Result<reqwest::Client, String> {
        let redirect = if self.follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        };

        let mut builder = reqwest::Client::builder()
            .redirect(redirect)
            .connect_timeout(self.connect_timeout)
            // Disable idle connection pooling: every request opens a fresh
            // connection. Required for the 421 (Misdirected Request) retry to
            // land on a *new* connection, as the fetch spec and the WPT
            // connection-pool tests expect (otherwise the retry reuses the same
            // keep-alive socket and the server counts only one connection).
            .pool_max_idle_per_host(0);

        // --- Cookie jar ---
        // The WHATWG `fetch` path disables reqwest's automatic store
        // (`cookie_jar = false`) and manages cookies itself via the shared
        // global jar, so the fetch `credentials` mode decides whether a cookie
        // is sent/stored (reqwest's auto-store would send them unconditionally).
        if self.cookie_jar {
            builder = builder.cookie_store(true);
        }

        // --- Proxy ---
        if let Some(ref proxy_url) = self.proxy {
            let mut proxy =
                reqwest::Proxy::all(proxy_url).map_err(|e| format!("invalid proxy: {e}"))?;
            if let Some(ref no_proxy) = self.no_proxy {
                proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
            }
            builder = builder.proxy(proxy);
        }
        // reqwest also reads http_proxy / https_proxy env vars by default.

        // --- TLS: skip validation (AIKE_TLS_INSECURE_HOSTS) ---
        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }

        // --- TLS: custom CA (per-instance option, legacy path) ---
        if let Some(ref pem) = self.root_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|e| format!("invalid CA cert: {e}"))?;
            builder = builder.add_root_certificate(cert);
        }

        // --- TLS: client certificate (mTLS) ---
        if let (Some(cert_pem), Some(key_pem)) = (&self.client_cert_pem, &self.client_key_pem) {
            // reqwest expects cert + key concatenated in one PEM blob.
            let mut buf = cert_pem.as_bytes().to_vec();
            buf.extend_from_slice(key_pem.as_bytes());
            let identity = reqwest::Identity::from_pem(&buf)
                .map_err(|e| format!("invalid mTLS identity: {e}"))?;
            builder = builder.identity(identity);
        }

        // --- DNS overrides ---
        for (host, addr) in &self.resolve_overrides {
            builder = builder.resolve(host, *addr);
        }

        // --- Protocol ---
        match self.protocol {
            ProtocolMode::Http1Only => {
                builder = builder.http1_only();
            }
            ProtocolMode::Http2PriorKnowledge => {
                builder = builder.http2_prior_knowledge();
            }
            ProtocolMode::Auto => {}
        }

        builder
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))
    }
}

/// The global cookie jar shared by WHATWG `fetch()` clients. Cookies persist
/// across separate `fetch()` calls, matching the browser's per-origin cookie
/// store. The fetch path reads/writes it manually according to the request's
/// `credentials` mode.
pub(crate) fn global_cookie_jar() -> Arc<reqwest::cookie::Jar> {
    static JAR: OnceLock<Arc<reqwest::cookie::Jar>> = OnceLock::new();
    JAR.get_or_init(|| Arc::new(reqwest::cookie::Jar::default()))
        .clone()
}

/// First-seen order of cookie names across the session, so the `Cookie` request
/// header lists cookies in creation order (WPT `cookies.any.js`). The
/// underlying `reqwest`/`cookie_store` jar returns cookies in hash order, which
/// does not match the creation order a browser UA cookie store preserves.
pub(crate) fn global_cookie_order() -> &'static std::sync::RwLock<Vec<String>> {
    static ORDER: OnceLock<std::sync::RwLock<Vec<String>>> = OnceLock::new();
    ORDER.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// Record the name of a cookie being stored, so it keeps its creation position
/// in the `Cookie` header. The name is the token before the first `=` or `;`.
pub(crate) fn cookie_order_record(set_cookie: &str) {
    let name = set_cookie
        .split(['=', ';'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if name.is_empty() {
        return;
    }
    let mut order = global_cookie_order().write().unwrap();
    if !order.iter().any(|n| n == &name) {
        order.push(name);
    }
}

/// Reorder a `Cookie` header value's `name=value` pairs into first-seen order.
/// Names not in the recorded order (e.g. overwritten/expired) are kept at the
/// end in their original relative order.
pub(crate) fn reorder_cookie_header(value: &str) -> String {
    let order = global_cookie_order().read().unwrap();
    let mut pairs: Vec<(usize, &str)> = value
        .split("; ")
        .map(|pair| {
            let name = pair.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
            let pos = order
                .iter()
                .position(|n| n == &name)
                .unwrap_or(usize::MAX);
            (pos, pair)
        })
        .collect();
    pairs.sort_by_key(|(pos, _)| *pos);
    pairs
        .into_iter()
        .map(|(_, pair)| pair)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Host patterns (either `host` or `host:port`) for which TLS certificate
/// validation is skipped. Populated once from the `AIKE_TLS_INSECURE_HOSTS`
/// env var, as a comma-separated list.
///
/// Each entry may contain `*` wildcards, matching any run of characters
/// (including dots), so `*.web-platform.test` covers every WPT subdomain and
/// `*.web-platform.test:8443` pins it to one port. Entries without a port
/// match any port.
///
/// This is a test/dev escape hatch for self-signed servers (e.g. WPT's
/// `web-platform.test:8443`). It is matched per-request against the target
/// URL's host/port, so validation stays on for every other host.
fn insecure_hosts() -> &'static Vec<String> {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<String>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        std::env::var("AIKE_TLS_INSECURE_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Glob match supporting `*` (any run of characters, including dots).
///
/// Greedy scan with backtracking to the last `*`: linear in practice and
/// allocation-free, which keeps this cheap enough to run per request.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0usize, 0usize);
    // Position just after the most recent `*`, and where it last matched up to.
    let (mut star, mut backtrack) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            backtrack = ti;
            pi += 1; // tentatively match zero characters
        } else if let Some(s) = star {
            // Mismatch: let the last `*` absorb one more character and retry.
            pi = s + 1;
            backtrack += 1;
            ti = backtrack;
        } else {
            return false;
        }
    }
    // Trailing `*`s may still match the empty remainder.
    p[pi..].iter().all(|&c| c == '*')
}

/// Returns true if the given host (with optional port) is allow-listed to skip
/// TLS certificate validation via `AIKE_TLS_INSECURE_HOSTS`.
///
/// A pattern containing `:` is matched against `host:port`; otherwise it is
/// matched against the bare host, so it applies to every port.
pub fn tls_insecure_for_host(host: &str, port: Option<u16>) -> bool {
    let host = host.to_ascii_lowercase();
    let with_port = port.map(|p| format!("{host}:{p}"));

    insecure_hosts().iter().any(|pat| {
        // Only treat the pattern as host:port when it pins an explicit port,
        // so a bare `*` still means "any host, any port".
        match (pat.rsplit_once(':'), &with_port) {
            (Some((_, port_pat)), Some(hp)) if !port_pat.contains('*') => glob_match(pat, hp),
            _ => glob_match(pat, &host),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_matches_wildcards() {
        // Exact and full wildcard.
        assert!(glob_match("web-platform.test", "web-platform.test"));
        assert!(glob_match("*", "anything.example"));
        assert!(!glob_match("web-platform.test", "not-web-platform.test"));

        // `*` spans dots, so it covers nested WPT subdomains.
        assert!(glob_match("*.web-platform.test", "www1.web-platform.test"));
        assert!(glob_match(
            "*.web-platform.test",
            "www1.www2.web-platform.test"
        ));
        assert!(glob_match(
            "*.web-platform.test",
            "xn--lve-6lad.web-platform.test"
        ));
        // A bare `*.` prefix requires at least one leading label.
        assert!(!glob_match("*.web-platform.test", "web-platform.test"));

        // Suffix patterns must not match a longer, different domain.
        assert!(!glob_match(
            "*.web-platform.test",
            "web-platform.test.evil.com"
        ));

        // host:port forms.
        assert!(glob_match(
            "*.web-platform.test:8443",
            "www.web-platform.test:8443"
        ));
        assert!(!glob_match(
            "*.web-platform.test:8443",
            "www.web-platform.test:9999"
        ));

        // Backtracking: repeated literals after a wildcard.
        assert!(glob_match("*test", "web-platform.test"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }
}
