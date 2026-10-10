//! Cairn's calls to an identity provider: discovery, its key set, the code exchange. One
//! client for all of them, so one place holds the rules: a request goes only to an origin
//! Cairn may send secrets to (https, or http to this machine), whatever the issuer's
//! discovery document advertises; no redirect is followed; each call ends within
//! `IDENTITY_CALL_DURATION_MAX`; and a response body is read only up to the body limit, so
//! an issuer cannot make Cairn hold an unbounded answer (PRACTICES, Explicit limits).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use cairn_schema::Limit;
use openidconnect::{AsyncHttpClient, HttpRequest, HttpResponse, http};
use url::Url;

use crate::limits::IDENTITY_CALL_DURATION_MAX;

/// The bytes of one identity-provider response: discovery documents, key sets, and token
/// responses are a few kilobytes, and the body limit is the nearest named limit
/// (decisions/2026-10-06-values-the-limits-table-does-not-name-take-the-nearest-named.md).
fn response_bytes_max() -> usize {
    usize::try_from(Limit::BodyBytes.max()).unwrap_or(usize::MAX)
}

/// The client for an identity provider's endpoints.
#[derive(Clone, Debug)]
pub(crate) struct Outbound {
    client: reqwest::Client,
}

/// Why a call to an identity provider failed.
#[derive(Debug)]
pub(crate) struct OutboundError(String);

impl fmt::Display for OutboundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OutboundError {}

impl Outbound {
    /// The client.
    pub(crate) fn new() -> Result<Self, String> {
        // Following redirects would let an issuer point Cairn's calls anywhere.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(IDENTITY_CALL_DURATION_MAX)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client })
    }

    /// One call.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, OutboundError> {
        let failed = |error: &dyn fmt::Display| OutboundError(error.to_string());
        let url: Url = request
            .uri()
            .to_string()
            .parse()
            .map_err(|error| failed(&error))?;
        if !is_trusted_origin(&url) {
            return Err(OutboundError(format!(
                "{url} is neither https nor this machine"
            )));
        }
        let request = reqwest::Request::try_from(request).map_err(|error| failed(&error))?;
        let mut response = self
            .client
            .execute(request)
            .await
            .map_err(|error| failed(&error))?;
        let mut answer = http::Response::builder().status(response.status());
        for (name, value) in response.headers() {
            answer = answer.header(name, value);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| failed(&error))? {
            if body.len() + chunk.len() > response_bytes_max() {
                return Err(OutboundError(format!("{url} answered past the body limit")));
            }
            body.extend_from_slice(&chunk);
        }
        answer.body(body).map_err(|error| failed(&error))
    }
}

impl<'c> AsyncHttpClient<'c> for Outbound {
    type Error = OutboundError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, OutboundError>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(self.send(request))
    }
}

/// An origin Cairn sends secrets to: https, or http to this machine.
pub(crate) fn is_trusted_origin(url: &Url) -> bool {
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain == "localhost",
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_go_only_to_https_or_this_machine() {
        let cases = [
            ("https://issuer.example", true),
            ("http://127.0.0.1:8080", true),
            ("http://[::1]:8080", true),
            ("http://localhost:8080", true),
            ("http://issuer.example", false),
            ("http://10.0.0.1", false),
            ("ftp://issuer.example", false),
        ];
        for (url, trusted) in cases {
            assert_eq!(is_trusted_origin(&url.parse().unwrap()), trusted, "{url}");
        }
    }
}
