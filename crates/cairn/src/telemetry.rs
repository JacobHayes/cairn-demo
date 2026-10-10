//! Structured logs and metrics (ARCHITECTURE, Observability; PRD Non-functional: Observable),
//! and the panic policy (PRACTICES, Errors, panics, and rejections).
//!
//! Logs are JSON lines on standard error, at info and above, each carrying the span it was
//! written in (a request's id, method, and endpoint; a patch's id). Metrics go through the
//! `metrics` facade to a Prometheus recorder, served at `/api/metrics` behind the auth layer: the
//! API's request counts and latencies by endpoint, MCP tool calls by tool, patch outcomes,
//! caught engine panics, derive durations, and store commit durations.

mod context;
mod metrics;
mod sessions;
pub(crate) use sessions::request_session;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::prelude::*;

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::limits::METRICS_UPKEEP_INTERVAL;

/// Installs the JSON log subscriber for the process; a second call does nothing.
pub fn init_logs() {
    use sentry::integrations::tracing::EventFilter;
    let sentry =
        sentry::integrations::tracing::layer().event_filter(|metadata| match *metadata.level() {
            tracing::Level::ERROR
                if metadata.target() == "cairn_api::engine_panic"
                    || metadata.target() == "cairn::panic" =>
            {
                EventFilter::Log | EventFilter::Breadcrumb
            }
            tracing::Level::ERROR => EventFilter::Event | EventFilter::Log,
            tracing::Level::WARN | tracing::Level::INFO => {
                EventFilter::Breadcrumb | EventFilter::Log
            }
            _ => EventFilter::Ignore,
        });
    let format = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(std::io::stderr)
        // A log that cannot be written (standard error closed under a supervisor) is lost,
        // never a panic, which would stop the process (PRACTICES: panics outside the engine
        // abort).
        .log_internal_errors(false);
    let installed = tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .with(sentry)
        .with(context::RequestContext)
        .with(format)
        .try_init();
    // Only a second call fails, and the first subscriber stands.
    drop(installed);
}

/// Installs the process's metrics recorder and starts its upkeep on the current runtime.
///
/// # Errors
///
/// When a recorder is already installed.
pub fn install_metrics() -> Result<PrometheusHandle, String> {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    ::metrics::set_global_recorder(metrics::Fanout(recorder)).map_err(|error| error.to_string())?;
    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(METRICS_UPKEEP_INTERVAL);
        loop {
            interval.tick().await;
            upkeep.run_upkeep();
        }
    });
    Ok(handle)
}

/// `GET /api/metrics`: the recorder's metrics in Prometheus's text format.
pub fn metrics_router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route("/api/metrics", get(render))
        .with_state(Arc::new(handle))
}

async fn render(State(handle): State<Arc<PrometheusHandle>>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        handle.render(),
    )
}

/// What a panic does to the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicAction {
    /// An engine call inside a request: the service catches it, the request fails with a
    /// 500, and the API logs and counts it.
    LeaveToTheService,
    /// Anywhere else (shared shell state, a background task, startup): log it and abort,
    /// since shared state may be inconsistent.
    Abort,
}

/// PRACTICES, Programmer errors panic: what a panic on this thread does.
#[must_use]
pub fn panic_action() -> PanicAction {
    if cairn_service::engine_call_active() {
        PanicAction::LeaveToTheService
    } else {
        PanicAction::Abort
    }
}

/// Installs the panic policy: a panic outside an engine call is logged and aborts the
/// process, rather than ending only the task it happened on as tokio would.
pub fn install_panic_policy() {
    std::panic::set_hook(Box::new(|info| {
        sentry::integrations::panic::panic_handler(info);
        flush();
        match panic_action() {
            PanicAction::LeaveToTheService => {}
            PanicAction::Abort => {
                let location = info.location().map(ToString::to_string).unwrap_or_default();
                tracing::error!(target: "cairn::panic", panic = %info, location, "a panic outside an engine call: aborting");
                // Not `eprintln!`, which panics if standard error is closed.
                let _ = writeln!(std::io::stderr(), "cairn: {info}");
                flush();
                std::process::abort();
            }
        }
    }));
}

/// The shared binary and browser release identity, embedded at build time.
pub const RELEASE: &str = env!("CAIRN_RELEASE");

/// Initializes telemetry before any runtime or durable state is opened.
///
/// # Errors
/// When a supplied DSN or trace sampling rate is invalid.
pub fn init_sentry() -> Result<sentry::ClientInitGuard, String> {
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|error| format!("SENTRY_DSN: {error}"))
        })
        .transpose()?;
    let rate = std::env::var("SENTRY_TRACES_SAMPLE_RATE")
        .unwrap_or_else(|_| "1.0".to_owned())
        .parse::<f32>()
        .map_err(|error| format!("SENTRY_TRACES_SAMPLE_RATE: {error}"))?;
    if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
        return Err("SENTRY_TRACES_SAMPLE_RATE must be between 0 and 1".to_owned());
    }
    let mut options = sentry::ClientOptions::new();
    options.dsn = dsn;
    options.environment = std::env::var("SENTRY_ENVIRONMENT").ok().map(Into::into);
    options.release = Some(RELEASE.into());
    options.attach_stacktrace = true;
    options.session_mode = sentry::SessionMode::Request;
    options.send_default_pii = true;
    options.shutdown_timeout = Duration::from_secs(2);
    // Logs are enabled by default in 0.49; the old enable_logs switch is deprecated.
    let options = options.traces_sampler(move |context| {
        let path = context.name().split_once(' ').map_or("", |(_, path)| path);
        if context.operation() == "http.server"
            && (!path.starts_with("/api/") || path == "/api/events/stream")
        {
            0.0
        } else {
            rate
        }
    });
    Ok(sentry::init(options))
}

/// Drains pending events, logs and metrics on normal, failed-closed and abort exits.
pub fn flush() {
    if let Some(client) = sentry::Hub::current().client() {
        client.flush(Some(Duration::from_secs(2)));
    }
}
