//! The auth layer the HTTP API and MCP endpoint sit behind (ARCHITECTURE, Auth; I2), and
//! the routes auth serves outside it: each provider's own, and sign-out.
//!
//! A request is authenticated by its session cookie and by every configured provider. Any
//! refusal refuses the request, so a credential that fails never falls through to another
//! provider (3.2, Security); otherwise the session, then the first provider in configured
//! order with a credential, names the actor. Handlers behind the layer read it as an
//! `Extension<Actor>`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use cairn_schema::Actor;
use cairn_store::AuthStore;
use url::Url;

use crate::accounts::Accounts;
use crate::cookies::{self, SESSION_COOKIE};
use crate::error::Refusal;
use crate::provider::{AuthProvider, BoxFuture, Listener, Peer, Presented, Verdict};

/// Authenticates a request the way the layer does; what a provider's own routes use to
/// learn who the browser is (the built-in OAuth server's consent).
pub trait Authenticate: Send + Sync {
    /// The actor a request's credentials name; none when it carries none.
    fn authenticate<'a>(
        &'a self,
        headers: &'a HeaderMap,
        extensions: &'a Extensions,
    ) -> BoxFuture<'a, Result<Option<Actor>, Refusal>>;
}

/// Who a browser is at a sign-in step (an OIDC sign-in, an OAuth consent).
pub(crate) struct Browser {
    /// The actor its credentials name, if any.
    pub actor: Option<Actor>,
    /// It carried a session cookie that has lapsed, which the step's answer clears.
    pub stale_session: bool,
}

/// Who a browser is at a sign-in step. A lapsed session cookie is what signing in
/// replaces, so the browser is asked again without it: a provider that signs each request
/// in (Tailscale, the dev user) still names its user rather than the browser passing for
/// no one. Any other refusal stands.
pub(crate) async fn browser(
    authenticator: &dyn Authenticate,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Result<Browser, Refusal> {
    match authenticator.authenticate(headers, extensions).await {
        Ok(actor) => Ok(Browser {
            actor,
            stale_session: false,
        }),
        Err(Refusal::Credential) if cookies::read(headers, SESSION_COOKIE).is_some() => {
            let without = cookies::without(headers, SESSION_COOKIE);
            let actor = authenticator.authenticate(&without, extensions).await?;
            Ok(Browser {
                actor,
                stale_session: true,
            })
        }
        Err(refusal) => Err(refusal),
    }
}

/// The deployment's auth: accounts and the configured providers.
pub struct Auth<S> {
    inner: Arc<Inner<S>>,
}

struct Inner<S> {
    accounts: Accounts<S>,
    providers: Vec<Arc<dyn AuthProvider>>,
    listener: Listener,
    secure: bool,
}

impl<S> Clone for Auth<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S: AuthStore + 'static> Auth<S> {
    /// Auth for a server on `listener` at the public `base` URL, asking `providers` in
    /// this order.
    #[must_use]
    pub fn new(
        accounts: Accounts<S>,
        listener: Listener,
        base: &Url,
        providers: Vec<Arc<dyn AuthProvider>>,
    ) -> Self {
        let inner = Inner {
            accounts,
            providers,
            listener,
            secure: base.scheme() == "https",
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// The accounts.
    #[must_use]
    pub fn accounts(&self) -> &Accounts<S> {
        &self.inner.accounts
    }

    /// `router` behind the auth layer: every request reaches it with an `Actor` extension,
    /// or is answered 401, 403, or 503 without reaching it. A TCP server must record peers
    /// (axum's `into_make_service_with_connect_info::<SocketAddr>`): a request with no
    /// recorded peer is never local, so the local-only providers refuse it.
    pub fn protect(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(
            self.clone(),
            require_actor::<S>,
        ))
    }

    /// The routes auth serves outside the layer: every provider's own, and sign-out.
    pub fn router(&self) -> Router {
        let authenticator: Arc<dyn Authenticate> = Arc::new(self.clone());
        let sign_out = Router::new()
            .route("/api/auth/sign-out", post(sign_out::<S>))
            .with_state(self.clone());
        let routers = self.inner.providers.iter();
        routers
            .filter_map(|provider| provider.router(Arc::clone(&authenticator)))
            .fold(sign_out, Router::merge)
    }

    /// The connection's peer, as the server recorded it.
    #[must_use]
    pub fn peer(&self, extensions: &Extensions) -> Peer {
        match (
            extensions.get::<ConnectInfo<SocketAddr>>(),
            self.inner.listener,
        ) {
            (Some(ConnectInfo(address)), Listener::Tcp(_)) => Peer::Tcp(*address),
            (_, Listener::Unix) => Peer::Unix,
            (None, Listener::Tcp(_)) => Peer::Unknown,
        }
    }

    /// The actor a request names: its session, or the first provider with a credential.
    ///
    /// # Errors
    ///
    /// The refusal of the session or of any provider.
    pub async fn authenticate_request(
        &self,
        request: Presented<'_>,
    ) -> Result<Option<Actor>, Refusal> {
        let mut found = self.session_actor(request.headers).await?;
        self.check_authorization(request)?;
        for provider in &self.inner.providers {
            let actor = match provider.authenticate(request).await {
                Verdict::Absent => continue,
                Verdict::Refused(refusal) => return Err(refusal),
                Verdict::Actor(actor) => actor,
                Verdict::Identity(_) if found.is_some() => continue,
                Verdict::Identity(identity) => {
                    let accounts = &self.inner.accounts;
                    let user = accounts.resolve(&identity, None, provider.auto_link());
                    Actor {
                        user: user.await?,
                        agent: None,
                    }
                }
            };
            found.get_or_insert(actor);
        }
        Ok(found)
    }

    /// The session cookie's user; a cookie that is not a live session is refused.
    async fn session_actor(&self, headers: &HeaderMap) -> Result<Option<Actor>, Refusal> {
        let Some(token) = cookies::read(headers, SESSION_COOKIE) else {
            return Ok(None);
        };
        match self.inner.accounts.session_user(&token).await? {
            Some(user) => Ok(Some(Actor { user, agent: None })),
            None => Err(Refusal::Credential),
        }
    }

    /// An `Authorization` header must hold a bearer token some provider claims: anything
    /// else is a credential that cannot be checked, so it is refused rather than ignored.
    fn check_authorization(&self, request: Presented<'_>) -> Result<(), Refusal> {
        if !request.headers.contains_key(AUTHORIZATION) {
            return Ok(());
        }
        let claimed = request.bearer().is_some_and(|token| {
            let mut providers = self.inner.providers.iter();
            providers.any(|provider| provider.claims_bearer(token))
        });
        if claimed {
            Ok(())
        } else {
            Err(Refusal::Credential)
        }
    }

    /// 401 with the `Bearer` challenge, naming the resource metadata where the built-in
    /// OAuth server serves it (I2), and clearing a session cookie that failed.
    fn challenge(&self, path: &str, error: Option<&str>, clear_session: bool) -> Response {
        let mut challenge = String::from("Bearer");
        let mut parameters: Vec<String> = error
            .map(|error| format!("error=\"{error}\""))
            .into_iter()
            .collect();
        let mut providers = self.inner.providers.iter();
        parameters.extend(providers.find_map(|provider| provider.challenge(path)));
        if !parameters.is_empty() {
            challenge.push(' ');
            challenge.push_str(&parameters.join(", "));
        }
        let mut response = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response.headers_mut().insert(WWW_AUTHENTICATE, value);
        }
        if clear_session {
            let cleared = cookies::clear(SESSION_COOKIE, "/", self.inner.secure);
            cookies::append(response.headers_mut(), cleared);
        }
        response
    }
}

impl<S: AuthStore + 'static> Authenticate for Auth<S> {
    fn authenticate<'a>(
        &'a self,
        headers: &'a HeaderMap,
        extensions: &'a Extensions,
    ) -> BoxFuture<'a, Result<Option<Actor>, Refusal>> {
        let request = Presented {
            headers,
            peer: self.peer(extensions),
        };
        Box::pin(self.authenticate_request(request))
    }
}

async fn require_actor<S: AuthStore + 'static>(
    State(auth): State<Auth<S>>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = Presented {
        headers: request.headers(),
        peer: auth.peer(request.extensions()),
    };
    let path = request.uri().path().to_owned();
    match auth.authenticate_request(presented).await {
        Ok(Some(actor)) => {
            let user = auth
                .inner
                .accounts
                .store()
                .user(&actor.user)
                .await
                .ok()
                .flatten();
            let identities = auth
                .inner
                .accounts
                .store()
                .identities_of(&actor.user)
                .await
                .unwrap_or_default();
            let email = identities
                .iter()
                .flat_map(|identity| &identity.verified_emails)
                .next()
                .map(ToString::to_string);
            let span = tracing::info_span!("authenticated", user.id = %actor.user,
                user.username = user.as_ref().map(|user| user.name.as_str()),
                user.email = email.as_deref());
            request.extensions_mut().insert(actor);
            tracing::Instrument::instrument(next.run(request), span).await
        }
        Ok(None) => auth.challenge(&path, None, false),
        Err(Refusal::Credential) => {
            let had_session = cookies::read(request.headers(), SESSION_COOKIE).is_some();
            auth.challenge(&path, Some("invalid_token"), had_session)
        }
        Err(Refusal::Peer) => (StatusCode::FORBIDDEN, Refusal::Peer.to_string()).into_response(),
        Err(refusal @ Refusal::Unavailable(_)) => {
            (StatusCode::SERVICE_UNAVAILABLE, refusal.to_string()).into_response()
        }
    }
}

/// Ends the browser's session and clears its cookie.
async fn sign_out<S: AuthStore + 'static>(
    State(auth): State<Auth<S>>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = cookies::read(&headers, SESSION_COOKIE)
        && let Err(error) = auth.inner.accounts.end_session(&token).await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let cleared = cookies::clear(SESSION_COOKIE, "/", auth.inner.secure);
    cookies::append(response.headers_mut(), cleared);
    response
}
