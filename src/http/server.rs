// Pure Rust HTTP service core. No JS awareness.
//
// Provides `spawn_http_service()` which starts an axum server with all routes
// registered directly — one match, no fallback, no secondary routing.
//
// All route handlers must be fully resolved (MethodRouter<()>) — state is
// provided at the call site via .with_state() before building RouteEntry.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{Router, routing::MethodRouter};
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;

/// A single route entry keyed by path.
/// Handler is a fully resolved MethodRouter<()> — state already provided via .with_state().
/// Multiple entries for the same path are merged automatically by axum's path_router.
pub(crate) struct RouteEntry {
    pub path: String,
    pub handler: MethodRouter<()>,
}

/// Handle returned by `spawn_http_service` for controlling the server lifecycle.
pub(crate) struct ServerHandle {
    pub shutdown_tx: oneshot::Sender<()>,
    pub is_running: Arc<AtomicBool>,
}

/// Spawn an axum HTTP service as a background tokio task.
///
/// All routes are registered directly into axum — one match, no fallback.
/// Each RouteEntry.handler must already have its state resolved via .with_state().
pub(super) fn spawn_http_server(
    routes: Vec<RouteEntry>,
    host: String,
    port: u16,
) -> Result<ServerHandle, String> {
    let mut router: Router = Router::new();

    // Register all routes — axum matches in one pass.
    // MethodRouter already encodes the HTTP method, so we use route() directly.
    for entry in routes {
        router = router.route(&entry.path, entry.handler);
    }

    router = router.layer(CorsLayer::permissive());

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
