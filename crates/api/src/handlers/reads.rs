//! The store-backed reads (ARCHITECTURE, HTTP API: Resources): whole documents by id, the
//! domain document, the journey index, route detail, search, and events.

use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, header::ETAG};
use cairn_schema::{
    Actor, Deployment, DomainDocument, Entity, EntityKey, Journey, JourneyId, Route, RouteId,
    RouteVersion, VersionNumber,
};
use cairn_service::ServiceError;
use cairn_store::Store;
use cairn_store::StoreError;

use super::Answer;
use crate::Api;
use crate::error::ApiError;
use crate::extract::{Path, segment};
use crate::query::{self, Params};
use crate::wire::{EventPage, JourneyPage, RouteDetail, RoutePage, SearchPage};

/// `GET /api/journeys` (C16).
pub async fn journeys<S: Store + 'static>(
    State(api): State<Api<S>>,
    RawQuery(raw): RawQuery,
) -> Answer<JourneyPage> {
    crate::chaos::available()?;
    let params = Params::parse(raw.as_deref(), query::JOURNEY_PARAMS)?;
    let page = api.service.journeys(&query::journeys(&params)?).await?;
    Ok(Json(JourneyPage {
        items: page.items.into_iter().map(Into::into).collect(),
        next: page.next,
    }))
}

/// `GET /api/journeys/{id}`.
pub async fn journey<S: Store + 'static>(
    State(api): State<Api<S>>,
    Path(id): Path<String>,
) -> Answer<Journey> {
    let id: JourneyId = segment(&id, "journey")?;
    let journey = api.service.journey(&id).await?;
    journey
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format_args!("journey {id}")))
}

/// `GET /api/journeys/{id}/document`: at the caller's today, with their entities as the viewer.
pub async fn document<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> Answer<DomainDocument> {
    let id: JourneyId = segment(&id, "journey")?;
    let document = api.service.document(&api.call(actor), &id).await?;
    document
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format_args!("journey {id}")))
}

/// `GET /api/routes` (I2).
pub async fn routes<S: Store + 'static>(
    State(api): State<Api<S>>,
    RawQuery(raw): RawQuery,
) -> Answer<RoutePage> {
    crate::chaos::available()?;
    let params = Params::parse(raw.as_deref(), query::ROUTE_PARAMS)?;
    let (kind, after, size) = query::routes(&params)?;
    let page = api.service.routes(kind, after.as_ref(), size).await?;
    Ok(Json(RoutePage {
        items: page.items.into_iter().map(Into::into).collect(),
        next: page.next,
    }))
}

/// `GET /api/routes/{id}`.
pub async fn route<S: Store + 'static>(
    State(api): State<Api<S>>,
    Path(id): Path<String>,
) -> Answer<Route> {
    let id: RouteId = segment(&id, "route")?;
    let route = api.service.route(&id).await?;
    route
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format_args!("route {id}")))
}

/// `GET /api/routes/{id}/versions` (C17).
pub async fn route_versions<S: Store + 'static>(
    State(api): State<Api<S>>,
    Path(id): Path<String>,
) -> Result<(HeaderMap, Json<RouteDetail>), ApiError> {
    let id: RouteId = segment(&id, "route")?;
    let detail = api.service.route_detail(&id).await?;
    let detail = detail.ok_or_else(|| ApiError::not_found(format_args!("route {id}")))?;
    // C17: a publication tag lets consumers identify the version represented by this page.
    let published = detail.versions.last().ok_or_else(|| {
        ServiceError::Store(StoreError::Backend(format!(
            "the publication index for route {id} has no latest version"
        )))
    })?;
    let mut headers = HeaderMap::new();
    let tag = format!("W/\"{}-{}\"", detail.revision, published.version);
    if let Ok(tag) = HeaderValue::from_str(&tag) {
        headers.insert(ETAG, tag);
    }
    Ok((headers, Json(detail.into())))
}

/// `GET /api/routes/{id}/versions/{version}`.
pub async fn route_version<S: Store + 'static>(
    State(api): State<Api<S>>,
    Path((id, version)): Path<(String, String)>,
) -> Answer<RouteVersion> {
    let id: RouteId = segment(&id, "route")?;
    let version: VersionNumber = segment(&version, "version")?;
    let found = api.service.route_version(&id, version).await?;
    found
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format_args!("version {version} of route {id}")))
}

/// `GET /api/deployment`.
pub async fn deployment<S: Store + 'static>(State(api): State<Api<S>>) -> Answer<Deployment> {
    Ok(Json(api.service.deployment().await?))
}

/// `GET /api/entities/{key}` (E6).
pub async fn entity<S: Store + 'static>(
    State(api): State<Api<S>>,
    Path(key): Path<String>,
) -> Answer<Entity> {
    let key: EntityKey = segment(&key, "entity")?;
    let entity = api.service.entity(&key).await?;
    entity
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format_args!("entity {key}")))
}

/// `GET /api/search`.
pub async fn search<S: Store + 'static>(
    State(api): State<Api<S>>,
    RawQuery(raw): RawQuery,
) -> Answer<SearchPage> {
    crate::chaos::search_ready().await;
    let params = Params::parse(raw.as_deref(), query::SEARCH_PARAMS)?;
    let page = api.service.search(&query::search(&params)?).await?;
    Ok(Json(page.into()))
}

/// `GET /api/events` (J5).
pub async fn events<S: Store + 'static>(
    State(api): State<Api<S>>,
    RawQuery(raw): RawQuery,
) -> Answer<EventPage> {
    let params = Params::parse(raw.as_deref(), query::EVENT_PARAMS)?;
    let page = api.service.events(&query::events(&params)?).await?;
    Ok(Json(page.into()))
}
