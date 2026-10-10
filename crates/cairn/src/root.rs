//! The composition root (ARCHITECTURE, Service layer and composition): one function that
//! builds the server from its configuration. The store is the caller's (Turso at the
//! configured path for `cairn serve`, memory for `cairn demo`); the notifier is the
//! in-process one, whose ticks the API's stream coalesces on tokio's timer; the auth providers are the configured ones, in order; the assistant is present exactly
//! when configured, a `None` here and absent from the capabilities otherwise (I5). The API
//! (with MCP at `/api/mcp`), auth's routes, `/api/metrics`, and the embedded UI share one router
//! behind the `Host` allowlist, served on one port.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use cairn_assistant::Assistant;
use cairn_assistant::protocol::HttpProvider;
use cairn_auth::{
    Accounts, Auth, AuthProvider, Clock, DevProvider, GcpIapProvider, OAuthServer, OidcProvider,
    TailscaleProvider,
};
use cairn_service::{AuthKind, AuthMethod, Capabilities, Parts, Service};
use cairn_store::{AuthStore, InProcessNotifier, Notifier, Store, StoreError};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;

use crate::assets::{self, Asset, WebBuildMissing};
use crate::config::{Config, Problem, Provider};
use crate::host::AllowedHosts;
use crate::{listener, telemetry};

/// What the root assembles a server from besides its configuration: the parts a test
/// replaces (the store, the notifier, the web build, the metrics recorder, the clock).
pub struct Assembly<S> {
    /// The store, shared by the service, auth, and the assistant.
    pub store: Arc<S>,
    /// The notifier every commit is announced through.
    pub notifier: Arc<dyn Notifier>,
    /// The web build.
    pub assets: Vec<Asset>,
    /// The metrics `/api/metrics` serves.
    pub metrics: PrometheusHandle,
    /// The deployment clock.
    pub clock: Clock,
}

/// Why the server did not start, or stopped.
#[derive(Debug)]
pub enum StartupError {
    /// An auth provider refused its configuration for this listener or URL.
    Providers(Vec<Problem>),
    /// The database could not be opened or migrated.
    Store(StoreError),
    /// The binary holds no web build.
    WebBuild(WebBuildMissing),
    /// The metrics recorder could not be installed.
    Metrics(String),
    /// The listen address could not be bound.
    Bind {
        /// The address.
        address: SocketAddr,
        /// Why.
        error: io::Error,
    },
    /// Serving failed.
    Serve(io::Error),
}

impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::Providers(problems) => {
                write!(formatter, "the auth providers cannot start:")?;
                for problem in problems {
                    write!(formatter, "\n  {problem}")?;
                }
                Ok(())
            }
            StartupError::Store(error) => write!(formatter, "the database: {error}"),
            StartupError::WebBuild(missing) => write!(formatter, "{missing}"),
            StartupError::Metrics(error) => write!(formatter, "the metrics recorder: {error}"),
            StartupError::Bind { address, error } => {
                write!(formatter, "cannot listen on {address}: {error}")
            }
            StartupError::Serve(error) => write!(formatter, "serving: {error}"),
        }
    }
}

impl std::error::Error for StartupError {}

/// The capabilities the configuration offers: its sign-in methods in order, the assistant
/// when configured, and MCP and SSE always (they are not optional on the server).
#[must_use]
pub fn capabilities(config: &Config) -> Capabilities {
    let auth = config
        .auth
        .iter()
        .map(|provider| {
            let (name, kind) = match provider {
                Provider::Dev(dev) => (&dev.name, AuthKind::Dev),
                Provider::Oidc(oidc) => (&oidc.name, AuthKind::Oidc),
                Provider::BuiltinOauth(oauth) => (&oauth.name, AuthKind::BuiltinOauth),
                Provider::Tailscale(tailscale) => (&tailscale.name, AuthKind::Tailscale),
                Provider::GcpIap(iap) => (&iap.name, AuthKind::GcpIap),
            };
            AuthMethod {
                name: name.clone(),
                kind,
            }
        })
        .collect();
    Capabilities::server(auth, config.assistant.is_some())
}

/// The configured auth providers over `accounts`, in order, each checked against the
/// listener and the public URL.
///
/// # Errors
///
/// Every provider that refuses its configuration (the dev provider off loopback without its
/// override, Tailscale's proxy mode on a listener others reach, an OIDC issuer over plain
/// http, an IAP audience that is not an IAP resource).
pub fn providers<S: AuthStore + 'static>(
    config: &Config,
    accounts: &Accounts<S>,
) -> Result<Vec<Arc<dyn AuthProvider>>, Vec<Problem>> {
    let listener = config.listener();
    let base = &config.public_url;
    let mut built: Vec<Arc<dyn AuthProvider>> = Vec::with_capacity(config.auth.len());
    let mut problems = Vec::new();
    for provider in &config.auth {
        let made: Result<Arc<dyn AuthProvider>, _> = match provider {
            Provider::Dev(dev) => {
                DevProvider::new(dev.clone(), listener).map(|made| Arc::new(made) as _)
            }
            Provider::Oidc(oidc) => OidcProvider::new(oidc.clone(), accounts.clone(), base)
                .map(|made| Arc::new(made) as _),
            Provider::BuiltinOauth(oauth) => {
                Ok(Arc::new(OAuthServer::new(oauth.clone(), accounts.clone(), base)) as _)
            }
            Provider::Tailscale(tailscale) => {
                TailscaleProvider::new(tailscale.clone(), listener).map(|made| Arc::new(made) as _)
            }
            Provider::GcpIap(iap) => GcpIapProvider::new(iap.clone(), accounts.clock().clone())
                .map(|made| Arc::new(made) as _),
        };
        match made {
            Ok(made) => built.push(made),
            Err(error) => problems.push(Problem::new(
                format!("auth.{}", error.provider),
                error.reason,
            )),
        }
    }
    if problems.is_empty() {
        Ok(built)
    } else {
        Err(problems)
    }
}

/// The whole server's router: the API with MCP, the assistant when configured, auth's
/// routes, `/api/metrics` behind the auth layer, and the UI, all behind the `Host` allowlist.
///
/// # Errors
///
/// A provider refusing its configuration, or a web build with no page.
pub fn app<S: Store + 'static>(
    config: &Config,
    parts: Assembly<S>,
) -> Result<Router, StartupError> {
    let service = Service::new(Parts {
        store: Arc::clone(&parts.store),
        notifier: parts.notifier,
        settings: config.settings(),
        capabilities: capabilities(config),
    });
    let accounts = Accounts::new(Arc::clone(&parts.store), parts.clock.clone());
    let providers = providers(config, &accounts).map_err(StartupError::Providers)?;
    let auth = Auth::new(accounts, config.listener(), &config.public_url, providers);
    let assistant = config.assistant.clone().map(|provider| {
        Assistant::new(
            service.clone(),
            Arc::clone(&parts.store),
            parts.clock.clone(),
            Arc::new(HttpProvider::new(provider)),
        )
    });
    let ui = assets::router(&parts.assets).map_err(StartupError::WebBuild)?;
    let metrics = auth.protect(telemetry::metrics_router(parts.metrics));
    // Metrics and the UI sit beside the API under its request limits: the in-flight limit
    // is per process, whatever the path. The UI answers every path the API does not keep.
    let app = cairn_api::router_beside(service, &auth, assistant, metrics, Some(ui));
    Ok(AllowedHosts::new(&config.public_url, config.listen)
        .guard(app)
        .layer(axum::middleware::from_fn(telemetry::request_session))
        .layer(sentry::integrations::tower::SentryHttpLayer::new().enable_transaction())
        .layer(sentry::integrations::tower::NewSentryLayer::new_from_top()))
}

/// `cairn serve` and `cairn demo`: assembles the server over `store` with the embedded web
/// build, `beside` served with it (the demo's own routes; none for `serve`), and serves it on
/// the configured address until `shutdown` resolves.
///
/// # Errors
///
/// Any [`StartupError`].
pub async fn serve<S: Store + 'static>(
    config: &Config,
    store: Arc<S>,
    beside: Router,
    metrics: PrometheusHandle,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), StartupError> {
    let app = app(
        config,
        Assembly {
            store: Arc::clone(&store),
            notifier: Arc::new(InProcessNotifier::new()),
            assets: assets::embedded(),
            metrics,
            clock: Clock::system(),
        },
    )?
    .merge(beside);
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|error| StartupError::Bind {
            address: config.listen,
            error,
        })?;
    let address = listener.local_addr().map_err(|error| StartupError::Bind {
        address: config.listen,
        error,
    })?;
    tracing::info!(
        %address,
        public_url = %config.public_url,
        database = %config.database.display(),
        "listening"
    );
    listener::warn_unsupported();
    // A store that has failed closed answers nothing more: the server stops, and exits
    // non-zero, so a supervisor restarts the process and the open recovers from disk
    // (decisions/2026-10-09-a-commit-the-store-cannot-settle-fails-it-closed.md).
    let closing = Arc::clone(&store);
    let stop = async move {
        tokio::select! {
            () = shutdown => {}
            () = failed_closed(closing.as_ref()) => {}
        }
    };
    let served = listener::serve(listener, app, stop)
        .await
        .map_err(StartupError::Serve);
    telemetry::flush();
    store.health().map_err(StartupError::Store)?;
    served
}

/// Resolves once `store` has failed closed. A health check reads the store's own state, so
/// looking once a second costs nothing.
async fn failed_closed<S: Store>(store: &S) {
    while store.health().is_ok() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
