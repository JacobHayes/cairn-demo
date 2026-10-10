//! Request sessions for release health (PRD Non-functional: Observable).
use std::sync::Arc;

use axum::{extract::Request, middleware::Next, response::Response};

/// Tracks each API request on the isolated hub installed by `NewSentryLayer`.
pub(crate) async fn request_session(request: Request, next: Next) -> Response {
    let session = request.uri().path().starts_with("/api/").then(|| {
        let hub = sentry::Hub::current();
        hub.start_session();
        RequestSession(hub)
    });
    let response = next.run(request).await;
    drop(session);
    response
}

struct RequestSession(Arc<sentry::Hub>);

impl Drop for RequestSession {
    fn drop(&mut self) {
        self.0.end_session();
    }
}
