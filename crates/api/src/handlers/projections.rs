//! The derived reads (ARCHITECTURE, HTTP API: Resources; I1, I3, C8): every projection of a
//! journey, derived by the service at the caller's today with their entities as the viewer,
//! answered with the revisions it came from; node detail with its explanation lists capped
//! and pageable; and history by page.

use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use axum::routing::get;
use cairn_schema::{
    Actor, DecisionView, Derived, ExplainedField, ExplanationPage, JourneyId, Level, ListPage,
    Next, NodeKey, Snapshot, StatusSummary, Timeline, Trace,
};
use cairn_store::Store;

use super::Answer;
use crate::Api;
use crate::extract::{Path, segment};
use crate::query::{self, Params};
use crate::wire::{History, Mine, NodeDetail, Projected};

/// The derived reads' endpoints and handlers.
pub fn served<S: Store + 'static>() -> crate::Served<S> {
    use crate::endpoints as at;
    vec![
        (&at::SNAPSHOT, get(snapshot::<S>)),
        (&at::LEVEL, get(level::<S>)),
        (&at::TRACE, get(trace::<S>)),
        (&at::DERIVED, get(derived::<S>)),
        (&at::DECISIONS, get(decisions::<S>)),
        (&at::TIMELINE, get(timeline::<S>)),
        (&at::SUMMARY, get(summary::<S>)),
        (&at::NEXT, get(next::<S>)),
        (&at::NODES, get(nodes::<S>)),
        (&at::MINE, get(mine::<S>)),
        (&at::NODE, get(node_detail::<S>)),
        (&at::EXPLANATIONS, get(explanations::<S>)),
        (&at::HISTORY, get(history::<S>)),
    ]
}

/// A projection's answer.
type Projection<T> = Answer<Projected<T>>;

fn journey(id: &str) -> Result<JourneyId, crate::error::ApiError> {
    segment(id, "journey")
}

fn node(key: &str) -> Result<NodeKey, crate::error::ApiError> {
    segment(key, "node")
}

/// `GET /api/journeys/{id}/snapshot` (I3).
pub async fn snapshot<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Projection<Snapshot> {
    let id = journey(&id)?;
    let params = Params::parse(raw.as_deref(), query::SNAPSHOT_PARAMS)?;
    let scope = query::snapshot(&params)?;
    let projected = api.service.snapshot(&api.call(actor), &id, &scope).await?;
    params.page_at(projected.revision)?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/level` (C2).
pub async fn level<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Projection<Level> {
    let id = journey(&id)?;
    let params = Params::parse(raw.as_deref(), query::LEVEL_PARAMS)?;
    let query = query::level(&params)?;
    let call = api.call(actor);
    let projected = api.service.level(&call, &id, &query).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/trace/{key}` (C7).
pub async fn trace<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path((id, key)): Path<(String, String)>,
) -> Projection<Trace> {
    let (id, key) = (journey(&id)?, node(&key)?);
    let projected = api.service.trace(&api.call(actor), &id, &key).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/derived` (D3): the whole derive, which the browser computes itself from
/// the domain document; for a client without the wasm engine.
pub async fn derived<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> Projection<Derived> {
    let id = journey(&id)?;
    let projected = api.service.derived(&api.call(actor), &id).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/decisions` (C12).
pub async fn decisions<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> Projection<DecisionView> {
    let id = journey(&id)?;
    let projected = api.service.decision_view(&api.call(actor), &id).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/timeline` (C13).
pub async fn timeline<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> Projection<Timeline> {
    let id = journey(&id)?;
    let projected = api.service.timeline(&api.call(actor), &id).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/summary` (C18).
pub async fn summary<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> Projection<StatusSummary> {
    let id = journey(&id)?;
    let projected = api.service.status_summary(&api.call(actor), &id).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/next` (C10).
pub async fn next<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Projection<Next> {
    let id = journey(&id)?;
    let params = Params::parse(raw.as_deref(), query::NEXT_PARAMS)?;
    let asked = query::next(&params)?;
    let top = query::next_top(&params)?;
    let mut projected = api.service.next(&api.call(actor), &id, &asked).await?;
    if let Some(top) = top {
        projected.value.items.drain(top..);
    }
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/nodes` (C9).
pub async fn nodes<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Projection<ListPage> {
    let id = journey(&id)?;
    let params = Params::parse(raw.as_deref(), query::LIST_PARAMS)?;
    let asked = query::list(&params)?;
    let projected = api.service.list(&api.call(actor), &id, &asked).await?;
    params.page_at(projected.revision)?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/mine` (E4).
pub async fn mine<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Projection<Mine> {
    let id = journey(&id)?;
    let params = Params::parse(raw.as_deref(), query::MINE_PARAMS)?;
    let kinds = query::mine(&params)?;
    let projected = api.service.mine(&api.call(actor), &id, &kinds).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/nodes/{key}` (C8): explanation lists capped with their totals.
pub async fn node_detail<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path((id, key)): Path<(String, String)>,
) -> Projection<NodeDetail> {
    let (id, key) = (journey(&id)?, node(&key)?);
    let projected = api.service.node_detail(&api.call(actor), &id, &key).await?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/nodes/{key}/explanations/{field}`: a page of one explanation list,
/// largest first; the first page is what node detail carries.
pub async fn explanations<S: Store + 'static>(
    State(api): State<Api<S>>,
    Extension(actor): Extension<Actor>,
    Path((id, key, field)): Path<(String, String, String)>,
    RawQuery(raw): RawQuery,
) -> Projection<ExplanationPage> {
    let (id, key) = (journey(&id)?, node(&key)?);
    let field: ExplainedField = segment(&field, "explained field")?;
    let params = Params::parse(raw.as_deref(), query::EXPLANATION_PARAMS)?;
    let cursor = params.cursor()?;
    let call = api.call(actor);
    let projected = api
        .service
        .explanations(&call, &id, &key, field, cursor)
        .await?;
    params.page_at(projected.revision)?;
    Ok(Json(Projected::from_service(projected)))
}

/// `GET /api/journeys/{id}/history` (J4).
pub async fn history<S: Store + 'static>(
    State(api): State<Api<S>>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Answer<History> {
    let id = journey(&id)?;
    let params = Params::parse(raw.as_deref(), query::HISTORY_PARAMS)?;
    let (node, after) = query::history(&params)?;
    let history = api.service.history(&id, node.as_ref(), after).await?;
    Ok(Json(history.into()))
}
