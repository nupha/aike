// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `HttpClientConfig` — client-level settings for the reusable `HttpClient`.
//!
//! Holds proxy, TLS, cookie, DNS, and connection-level options.
//! `build_http_client()` constructs a `reqwest::Client` from the config.

use std::net::SocketAddr;
use std::time::Duration;

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
        }
    }
}

impl HttpClientConfig {
    /// Build a `reqwest::Client` from the configuration.
    pub fn build_http_client(&self) -> Result<reqwest::Client, String> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(self.connect_timeout);

        // --- Cookie jar ---
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

        // --- TLS: skip validation ---
        if self.danger_accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }

        // --- TLS: custom CA ---
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
