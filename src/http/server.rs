// Pure Rust HTTP service core. No JS awareness.
//
// Provides `spawn_http_server()` which starts an axum server with all routes
// registered directly — one match, no fallback, no secondary routing.
//
// All route handlers must be fully resolved (MethodRouter<()>) — state is
// provided at the call site via .with_state() before adding routes.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::Router;
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;

/// Handle returned by `spawn_http_server` for controlling the server lifecycle.
pub(crate) struct ServerHandle {
    pub shutdown_tx: oneshot::Sender<()>,
    pub is_running: Arc<AtomicBool>,
}

/// Spawn an axum HTTP service as a background tokio task.
///
/// The caller composes the router via axum-native merge/nest/route,
/// with all handlers already state-resolved via `.with_state()`.
pub(super) fn spawn_http_server(
    router: Router,
    host: String,
    port: u16,
) -> Result<ServerHandle, String> {
    let router = router.layer(CorsLayer::permissive());

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let is_running = Arc::new(AtomicBool::new(true));
    let is_running_clone = is_running.clone();

    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("HttpServer failed to bind {host}:{port}: {e}");
                is_running_clone.store(false, Ordering::Release);
                return;
            }
        };

        log::info!("HttpServer listening on {host}:{port}");

        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async { drop(shutdown_rx.await) })
            .await;

        is_running_clone.store(false, Ordering::Release);
    });

    Ok(ServerHandle {
        shutdown_tx,
        is_running,
    })
}
