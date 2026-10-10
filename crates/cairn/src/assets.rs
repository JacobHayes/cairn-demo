//! The embedded web build (ARCHITECTURE, Build, run, deploy): the app's Vite build, with
//! the wasm module, built by `mise run build:web` before `cargo build` into
//! `web/app/dist/build/` (never checked in) and embedded with `rust-embed`. Each file is
//! served at its own path, and `index.html` at `/` and at every other path the server does
//! not keep, since the UI routes by path: a screen's address loads the page, which shows
//! that screen (decisions/2026-10-08-the-api-is-served-under-api-and-the-app-owns-the-rest.md).
//!
//! A binary built without the web build embeds nothing, and `cairn serve` refuses to start
//! rather than serve an API with no UI ([`router`]).

use std::borrow::Cow;
use std::fmt;

use axum::Router;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};

/// The app's page.
pub const INDEX: &str = "index.html";

/// The web build, embedded from `web/app/dist/build/` in every build, debug ones included
/// (rust-embed's `debug-embed`): a debug build that read the folder at run time would read
/// it from the absolute path it was compiled at, and a compile restored from the build cache
/// may have been made in another checkout.
#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/app/dist/build/"]
#[allow_missing = true]
#[exclude = "*.map"]
struct WebBuild;

/// The web build's digest from build.rs: reading it makes the folder's contents an input of
/// this crate's compile, so a build cache keyed on the compile's inputs (mbx) recompiles
/// when the web build appears or changes rather than restoring the old embedding.
const _: &str = env!("CAIRN_WEB_BUILD_DIGEST");

/// One file of the web build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    /// Its path under the build, `/`-separated (`index.html`, `assets/index-1a2b.js`).
    pub path: String,
    /// Its bytes.
    pub bytes: Cow<'static, [u8]>,
    /// Its media type.
    pub media_type: String,
}

/// Every file of the embedded web build.
#[must_use]
pub fn embedded() -> Vec<Asset> {
    WebBuild::iter()
        .filter_map(|path| {
            let file = WebBuild::get(&path)?;
            Some(Asset {
                media_type: file.metadata.mimetype().to_owned(),
                bytes: file.data,
                path: path.into_owned(),
            })
        })
        .collect()
}

/// The binary holds no web build: it was built without `mise run build:web` first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebBuildMissing;

impl fmt::Display for WebBuildMissing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "this binary holds no web build ({INDEX} is missing): build it with `mise run build`"
        )
    }
}

impl std::error::Error for WebBuildMissing {}

/// The UI's routes: each asset at its path, and the page at `/` and, for `GET` and `HEAD`,
/// at any other path, a screen's address (the API's router hands this router only the paths
/// the server does not keep). Hashed bundles under `assets/` never change at their path, so
/// browsers keep them, and one missing there is not found rather than answered with the
/// page; the page is revalidated, so a new deployment's page is fetched at once
/// (ARCHITECTURE, Web UI: version skew).
///
/// # Errors
///
/// When `assets` holds no page.
pub fn router(assets: &[Asset]) -> Result<Router, WebBuildMissing> {
    let page = assets
        .iter()
        .find(|asset| asset.path == INDEX)
        .map(Served::new)
        .ok_or(WebBuildMissing)?;
    let mut router = Router::new().route("/", page.clone().handler());
    for asset in assets {
        router = router.route(&format!("/{}", asset.path), Served::new(asset).handler());
    }
    let screen = move |method: Method, uri: Uri| {
        let page = page.clone();
        async move {
            let bundle = uri.path().starts_with(&format!("/{BUNDLES}"));
            if bundle || !(method == Method::GET || method == Method::HEAD) {
                StatusCode::NOT_FOUND.into_response()
            } else {
                page.response()
            }
        }
    };
    Ok(router.fallback(screen))
}

/// Where the hashed bundles sit in the build.
const BUNDLES: &str = "assets/";

/// One asset as it is answered: its media type and caching, and its bytes.
#[derive(Clone)]
struct Served {
    headers: [(HeaderName, HeaderValue); 2],
    bytes: Cow<'static, [u8]>,
}

impl Served {
    fn new(asset: &Asset) -> Self {
        let cache = if asset.path.starts_with(BUNDLES) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let media_type = HeaderValue::from_str(&asset.media_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream"));
        Self {
            headers: [
                (CONTENT_TYPE, media_type),
                (CACHE_CONTROL, HeaderValue::from_static(cache)),
            ],
            bytes: asset.bytes.clone(),
        }
    }

    fn response(&self) -> Response {
        let mut response = (self.headers.clone(), self.bytes.clone()).into_response();
        if self.headers[0]
            .1
            .to_str()
            .is_ok_and(|value| value.starts_with("text/html"))
        {
            response
                .headers_mut()
                .insert("document-policy", HeaderValue::from_static("js-profiling"));
        }
        response
    }

    fn handler(self) -> MethodRouter {
        get(move || {
            let served = self.clone();
            async move { served.response() }
        })
    }
}
