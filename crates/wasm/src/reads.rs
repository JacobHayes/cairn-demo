//! The in-browser root's reads for the screens around a journey (brief 5.5): the journey
//! index filtered as `GET /api/journeys` filters it (C16), the route index and route detail (C17),
//! route files exported and imported through the service (A13), and the caller with their
//! identities and entities (H3). Each answers the API's JSON for the same request, so the
//! app's one data layer reads either host.

use std::collections::BTreeSet;

use cairn_schema::{
    AgentId, Domain, Email, EntityKey, GraphId, InsertionKey, JourneyId, JourneyStatus, Markdown,
    PatchId, Revision, RouteFile, RouteHeader, RouteId, Slug, Timestamp, Title, UserId,
    VersionNumber,
};
use cairn_service::WriteError;
use cairn_store::{JourneyQuery, PageSize};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::wasm_bindgen;

use crate::error::{HostError, json, read};
use crate::now_or_never;
use crate::root::{BrowserRoot, JourneyPage, JourneySummary, PatchAnswer, failed};

/// `GET /api/journeys`'s query (C16) as JSON: the statuses, the lineage route and version, the
/// entities referred to (E6), whether an upgrade is available, and the page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyIndexQuery {
    /// Journeys in any of these statuses; none for any status.
    #[serde(default)]
    pub status: BTreeSet<JourneyStatus>,
    /// Journeys following this route.
    #[serde(default)]
    pub route: Option<RouteId>,
    /// Journeys on this version of their route.
    #[serde(default)]
    pub version: Option<VersionNumber>,
    /// Journeys referring to any of these entities, directly or through an alias.
    #[serde(default)]
    pub referencing: BTreeSet<EntityKey>,
    /// Journeys whose route has, or has not, published a version newer than theirs.
    #[serde(default)]
    pub upgrade_available: Option<bool>,
    /// The page starts after this journey.
    #[serde(default)]
    pub after: Option<JourneyId>,
    /// The page's size, cut to the page limit; the limit when none.
    #[serde(default)]
    pub size: Option<u32>,
}

impl JourneyIndexQuery {
    /// The store's query, as the API reads its parameters.
    fn store_query(&self) -> JourneyQuery {
        JourneyQuery {
            statuses: self.status.clone(),
            route: self.route.clone(),
            version: self.version,
            referencing: (!self.referencing.is_empty()).then(|| self.referencing.clone()),
            upgrade_available: self.upgrade_available,
            after: self.after.clone(),
            size: self.size.map_or(PageSize::MAX, PageSize::new),
        }
    }
}

/// A route in the route index, as the API's `RouteSummary`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSummary {
    /// The route's fields.
    pub header: RouteHeader,
    /// Its revision.
    pub revision: Revision,
    /// Its latest published version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<VersionNumber>,
    /// Whether it has an open draft.
    pub draft_open: bool,
}

/// A page of the route index, as the API's `RoutePage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePage {
    /// The routes, in id order.
    pub items: Vec<RouteSummary>,
    /// Pass as `after` for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<RouteId>,
}

/// One published version and the journeys on it, as the API's `VersionJourneys`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionJourneys {
    /// The version.
    pub version: VersionNumber,
    /// When it was published.
    pub published_at: Timestamp,
    /// The journeys whose lineage is this version.
    pub journeys: BTreeSet<JourneyId>,
    /// Where this version of a segment is inserted (C19).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub insertions: Vec<InsertionUse>,
}

/// One insertion of a segment version, as the API's `InsertionUse` (C19).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertionUse {
    /// The domain holding it.
    pub host: Domain,
    /// The graph holding it.
    pub graph: GraphId,
    /// The insertion.
    pub insertion: InsertionKey,
    /// The root member's current title.
    pub title: Title,
    /// Whether the segment has a later published version.
    pub upgrade_available: bool,
}

/// C17: a route's versions with the journeys on each, as the API's `RouteDetail`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteDetail {
    /// The route's fields.
    pub header: RouteHeader,
    /// Its revision.
    pub revision: Revision,
    /// Its versions, oldest first.
    pub versions: Vec<VersionJourneys>,
}

/// `POST /api/routes/{id}/import`'s body (A13), as the API's `RouteImport`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteImport {
    /// The import's patch id (H5).
    pub patch_id: PatchId,
    /// The file, as JSON; its route is the one imported into.
    pub file: RouteFile,
    /// A note for each of its events (J1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Markdown>,
}

/// One identity the caller signs in with, as the API's `LinkedIdentity`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkedIdentity {
    /// The provider, by its configured name.
    pub provider: Slug,
    /// The provider's subject for the account.
    pub subject: Title,
    /// The emails the provider marked verified at the last sign-in (H3).
    pub verified_emails: BTreeSet<Email>,
    /// When it was linked.
    pub linked_at: Timestamp,
}

/// The caller (H2) and the entities their verified emails name (H3), as the API's `Viewer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewer {
    /// The user.
    pub user: UserId,
    /// The agent acting for them: never, in the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    /// Their entities, which "mine" covers together.
    pub entities: BTreeSet<EntityKey>,
    /// H3: present when their emails name more than one entity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_offer: Option<BTreeSet<EntityKey>>,
    /// The identities they sign in with, by provider and subject.
    pub identities: Vec<LinkedIdentity>,
}

impl BrowserRoot {
    /// C16: the journey index `query` asks for, as `GET /api/journeys` answers it.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub fn journey_index(&self, query: &JourneyIndexQuery) -> Result<JourneyPage, HostError> {
        let page = now_or_never(self.service().journeys(&query.store_query())).map_err(failed)?;
        let items = page.items.into_iter().map(|summary| {
            let upgrade_available = summary.upgrade_available();
            let cairn_store::JourneySummary {
                id,
                name,
                status,
                lineage,
                revision,
                created_at,
                latest_version,
                latest_version_node_count,
            } = summary;
            JourneySummary {
                id,
                name,
                status,
                lineage,
                revision,
                created_at,
                latest_version,
                latest_version_node_count,
                upgrade_available,
            }
        });
        Ok(JourneyPage {
            items: items.collect(),
            next: page.next,
        })
    }

    /// The route index: every route and segment, in id order, as `GET /api/routes` answers its
    /// first page at the page limit.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub fn route_page(&self, after: Option<&RouteId>) -> Result<RoutePage, HostError> {
        let page =
            now_or_never(self.service().routes(None, after, PageSize::MAX)).map_err(failed)?;
        let items = page.items.into_iter().map(|summary| RouteSummary {
            header: summary.header,
            revision: summary.revision,
            latest_version: summary.latest_version,
            draft_open: summary.draft_open,
        });
        Ok(RoutePage {
            items: items.collect(),
            next: page.next,
        })
    }

    /// C17: route `id`'s versions with the journeys on each.
    ///
    /// # Errors
    ///
    /// [`HostError::Missing`] when there is no such route; when the store fails.
    pub fn detail_of(&self, id: &RouteId) -> Result<RouteDetail, HostError> {
        let Some(detail) = now_or_never(self.service().route_detail(id)).map_err(failed)? else {
            return Err(HostError::Missing {
                message: format!("no route {id}"),
            });
        };
        let versions = detail.versions.into_iter().map(|version| VersionJourneys {
            version: version.version,
            published_at: version.published_at,
            journeys: version.journeys,
            insertions: version
                .insertions
                .into_iter()
                .map(|held| InsertionUse {
                    host: held.host,
                    graph: held.graph,
                    insertion: held.insertion,
                    title: held.title,
                    upgrade_available: held.upgrade_available,
                })
                .collect(),
        });
        Ok(RouteDetail {
            header: detail.header,
            revision: detail.revision,
            versions: versions.collect(),
        })
    }

    /// The caller: the local user, its identity, and the entities its emails name (H3).
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub fn viewer_now(&self, now: &str) -> Result<Viewer, HostError> {
        let call = self.call(now)?;
        let viewer = now_or_never(self.service().viewer(&call)).map_err(failed)?;
        let merge_offer = viewer.merge_offer().cloned();
        let identities = viewer.identities.into_iter().map(|record| LinkedIdentity {
            provider: record.provider,
            subject: record.subject,
            verified_emails: record.verified_emails,
            linked_at: record.linked_at,
        });
        Ok(Viewer {
            user: viewer.user,
            agent: None,
            entities: viewer.entities,
            merge_offer,
            identities: identities.collect(),
        })
    }

    /// A13: `request`'s file imported as the local user at `now`.
    ///
    /// # Errors
    ///
    /// [`HostError::Rejected`] with every violation of the file or the draft, or for a stale
    /// or reused patch; [`HostError::Failed`] when the store fails.
    pub fn import_file(&self, request: &RouteImport, now: &str) -> Result<PatchAnswer, HostError> {
        let call = self.call(now)?;
        let written = now_or_never(self.service().import_route(
            &call,
            request.patch_id.clone(),
            &request.file,
            request.note.clone(),
        ));
        match written {
            Ok(written) => Ok(written.into()),
            Err(WriteError::Rejected(rejection)) => Err(HostError::Rejected { rejection }),
            Err(WriteError::Failed(error)) => Err(failed(error)),
        }
    }
}

/// A route id read from JavaScript's text.
fn route_id(route: &str) -> Result<RouteId, HostError> {
    route
        .parse()
        .map_err(|error| HostError::unreadable("route", format!("{error:?}")))
}

#[wasm_bindgen]
impl BrowserRoot {
    /// C16: the journey index `query` (the JSON of a [`JourneyIndexQuery`]) asks for, as
    /// `GET /api/journeys` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`].
    #[wasm_bindgen(js_name = journeyIndex)]
    pub fn journey_index_json(&self, query: &str) -> Result<String, String> {
        let query: JourneyIndexQuery = read("journey query", query)?;
        Ok(json(&self.journey_index(&query)?))
    }

    /// The route index from after `after` (empty for the first page), as `GET /api/routes`
    /// answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`].
    pub fn routes(&self, after: &str) -> Result<String, String> {
        let after = if after.is_empty() {
            None
        } else {
            Some(route_id(after)?)
        };
        Ok(json(&self.route_page(after.as_ref())?))
    }

    /// C17: route `route`'s versions with the journeys on each, as
    /// `GET /api/routes/{id}/versions` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`]: no such route, or an unreadable input.
    #[wasm_bindgen(js_name = routeDetail)]
    pub fn route_detail(&self, route: &str) -> Result<String, String> {
        Ok(json(&self.detail_of(&route_id(route)?)?))
    }

    /// A13: version `version` of route `route` as a file, or its draft when `version` is
    /// empty, as `GET /api/routes/{id}/export` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`]: no such route, version, or draft, or an unreadable input.
    #[wasm_bindgen(js_name = exportFile)]
    pub fn export_file(&self, route: &str, version: &str) -> Result<String, String> {
        let id = route_id(route)?;
        let number: Option<VersionNumber> = if version.is_empty() {
            None
        } else {
            Some(read("version", version)?)
        };
        match now_or_never(self.service().export_route(&id, number)).map_err(failed)? {
            Some(file) => Ok(json(&file)),
            None => Err(HostError::Missing {
                message: format!("route {id} has no such version or draft"),
            }
            .into()),
        }
    }

    /// A13: imports a route file as the local user at `now`: `request` is the JSON of a
    /// [`RouteImport`]; the answer is the JSON of the patch answer, as
    /// `POST /api/routes/{id}/import` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`], the rejection among them.
    #[wasm_bindgen(js_name = importFile)]
    pub fn import_file_json(&self, request: &str, now: &str) -> Result<String, String> {
        let request: RouteImport = read("route import", request)?;
        Ok(json(&self.import_file(&request, now)?))
    }

    /// The caller at `now`, as `GET /api/users/me` answers it (H3).
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`].
    pub fn viewer(&self, now: &str) -> Result<String, String> {
        Ok(json(&self.viewer_now(now)?))
    }
}
