//! The in-browser root (ARCHITECTURE, Service layer and composition; Web UI: in-browser
//! host): the browser host's composition root. The same service as the server's, over the
//! memory store seeded from the fixtures on every load, with the in-process notifier, one
//! local identity, and no assistant, MCP, or SSE. Nothing persists and each page is its own.
//!
//! Its answers are the API's JSON for the same operations (a patch answer, a journey page,
//! a revision tick), so a data layer reads either host with one set of types. The memory
//! store answers at once, so each operation runs to completion synchronously; a subscriber
//! takes its ticks when its host's loop asks, as the SSE stream does on the server.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use cairn_schema::{
    Actor, Consequences, Domain, JourneyId, JourneyStatus, Lineage, Markdown, NodeKey, Notice,
    Patch, PatchEvents, PatchReceipt, ProposalId, RankConstants, Revision, RevisionOf, RouteId,
    Timestamp, Title, VersionNumber,
};
use cairn_service::{
    Call, Capabilities, DeploymentSettings, DomainPatch, Parts, Service, WriteError, Written,
};
use cairn_store::{InProcessNotifier, MemoryStore, Subscription, Take, Watch};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::wasm_bindgen;

use crate::error::{HostError, json, read};
use crate::fixtures::{seed, sign_in_local};
use crate::now_or_never;

/// `POST /api/{domain}/patches`'s body: a patch and the note its events carry (J1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    /// The patch.
    pub patch: Patch,
    /// A note for each of its events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Markdown>,
}

/// What an accepted patch answers (A17, H5, D7), as the API's `PatchAnswer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatchAnswer {
    /// Committed now.
    Applied {
        /// The receipt.
        receipt: PatchReceipt,
        /// D7: what it newly caused, by journey.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        consequences: BTreeMap<JourneyId, Consequences>,
        /// A20: advisory notices about the route graph an import or publish leaves.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notices: Vec<Notice>,
    },
    /// The patch id was committed before with the same content (H5).
    AlreadyApplied {
        /// The original receipt.
        receipt: PatchReceipt,
    },
}

impl From<Written> for PatchAnswer {
    fn from(written: Written) -> Self {
        match written {
            Written::Applied {
                receipt,
                consequences,
                notices,
            } => PatchAnswer::Applied {
                receipt,
                consequences,
                notices,
            },
            Written::AlreadyApplied { receipt } => PatchAnswer::AlreadyApplied { receipt },
        }
    }
}

/// A journey in the index (C16), as the API's `JourneySummary`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneySummary {
    /// The journey.
    pub id: JourneyId,
    /// Its name.
    pub name: Title,
    /// Its status.
    pub status: JourneyStatus,
    /// The route version it follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Lineage>,
    /// Its revision.
    pub revision: Revision,
    /// When it was created.
    pub created_at: Timestamp,
    /// The latest version its route has published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<VersionNumber>,
    /// The number of nodes in the latest published route version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version_node_count: Option<u32>,
    /// Whether its route has published a version newer than its own (C16).
    pub upgrade_available: bool,
}

/// A page of the journey index, as the API's `JourneyPage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyPage {
    /// The journeys, in id order.
    pub items: Vec<JourneySummary>,
    /// Pass as `after` for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<JourneyId>,
}

/// One revision announcement (H6), as the API's `Tick`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tick {
    /// The domain or proposal.
    pub of: RevisionOf,
    /// Its revision; 0 for one that does not exist.
    pub revision: Revision,
}

/// What a subscriber takes (H6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "take", rename_all = "snake_case", deny_unknown_fields)]
pub enum Taken {
    /// The first take: the current revision of everything watched (complete, so a domain
    /// missing from it no longer exists).
    Current {
        /// The revisions.
        ticks: Vec<Tick>,
    },
    /// What moved since the last take, the latest revision of each.
    Ticks {
        /// The revisions.
        ticks: Vec<Tick>,
    },
    /// Something is held, but the last take was within `sse_coalescing_interval`: take again
    /// at `until_ms` on the same clock.
    Wait {
        /// When, in milliseconds on the caller's clock.
        until_ms: f64,
    },
    /// Nothing new.
    Empty,
}

/// A watch as the API's stream names it: `deployment`, `journey:<id>`, `route:<id>`,
/// `proposal:<id>`, or every one of a kind: `journeys`, `routes`, `proposals`.
fn watch(name: &str) -> Result<Watch, HostError> {
    let unreadable = |message: String| HostError::unreadable("watch", message);
    Ok(match name {
        "journeys" => Watch::Journeys,
        "routes" => Watch::Routes,
        "proposals" => Watch::Proposals,
        "deployment" => Watch::One(RevisionOf::Domain(Domain::Deployment)),
        _ => match name.split_once(':') {
            Some(("journey", id)) => Watch::One(RevisionOf::Domain(Domain::Journey(
                id.parse().map_err(|error| unreadable(format!("{error}")))?,
            ))),
            Some(("route", id)) => Watch::One(RevisionOf::Domain(Domain::Route(
                id.parse().map_err(|error| unreadable(format!("{error}")))?,
            ))),
            Some(("proposal", id)) => Watch::One(RevisionOf::Proposal(
                id.parse::<ProposalId>()
                    .map_err(|error| unreadable(format!("{error}")))?,
            )),
            _ => return Err(unreadable(format!("{name:?} names nothing to watch"))),
        },
    })
}

/// A service failure, as the root reports it.
pub(crate) fn failed(error: impl std::fmt::Display) -> HostError {
    HostError::Failed {
        message: error.to_string(),
    }
}

/// J4: `GET /api/journeys/{id}/history`'s answer, the API's `History`: a page of events grouped
/// by patch, and where the next page starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryAnswer {
    /// At most a page of events, grouped by patch, in log order.
    pub patches: Vec<PatchEvents>,
    /// Pass as `after` for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

/// A log position from JavaScript's number: none when negative, else a whole number within
/// the integers a double holds exactly.
fn position(after: f64) -> Result<Option<u64>, HostError> {
    const EXACT_MAX: f64 = 9_007_199_254_740_991.0;
    if after < 0.0 {
        return Ok(None);
    }
    if after.fract() != 0.0 || after > EXACT_MAX {
        return Err(HostError::unreadable(
            "after",
            format!("{after} is not a log position"),
        ));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a whole number from 0 to 2^53 - 1, checked above"
    )]
    Ok(Some(after as u64))
}

/// The browser host's composition root.
#[wasm_bindgen]
#[derive(Debug)]
pub struct BrowserRoot {
    service: Service<MemoryStore>,
    local: Actor,
}

impl BrowserRoot {
    /// The root with every fixture seeded (ARCHITECTURE, Web UI: fixtures seed it on every
    /// load). The deployment's zone is UTC: the service reads no zone database, and the
    /// browser host bundles none.
    ///
    /// # Errors
    ///
    /// When seeding fails, which is a bug in the fixtures or the service.
    pub fn seeded() -> Result<Self, HostError> {
        let store = Arc::new(MemoryStore::new());
        let settings = DeploymentSettings::new(
            "UTC"
                .parse()
                .map_err(|error| failed(format!("{error:?}")))?,
            TimeZone::UTC,
            RankConstants::default(),
        );
        let service = Service::new(Parts {
            store: Arc::clone(&store),
            notifier: Arc::new(InProcessNotifier::new()),
            settings,
            capabilities: Capabilities::browser(),
        });
        now_or_never(seed(&service)).map_err(failed)?;
        let at = "2026-09-01T12:00:00Z"
            .parse()
            .map_err(|error| failed(format!("{error:?}")))?;
        let local = now_or_never(sign_in_local(&*store, at)).map_err(failed)?;
        Ok(Self { service, local })
    }

    /// The service, for a native host or test that drives it directly.
    #[must_use]
    pub fn service(&self) -> &Service<MemoryStore> {
        &self.service
    }

    /// The local identity's call at `now` (an RFC 3339 timestamp).
    pub(crate) fn call(&self, now: &str) -> Result<Call, HostError> {
        Ok(Call {
            actor: self.local.clone(),
            now: now
                .parse()
                .map_err(|error| HostError::unreadable("now", error))?,
        })
    }

    /// A patch as the local user at `now`: the API's patch answer, or the rejection.
    ///
    /// # Errors
    ///
    /// [`HostError::Rejected`] for a stale, invalid, or reused patch; [`HostError::Unreadable`]
    /// for a proposal's lifecycle, which has its own operations.
    pub fn submit(&self, request: &PatchRequest, now: &str) -> Result<PatchAnswer, HostError> {
        let call = self.call(now)?;
        let submitted = DomainPatch::new(request.patch.clone(), request.note.clone())
            .map_err(|error| HostError::unreadable("patch", format!("{error:?}")))?;
        match now_or_never(self.service.patch(&call, &submitted)) {
            Ok(written) => Ok(written.into()),
            Err(WriteError::Rejected(rejection)) => Err(HostError::Rejected { rejection }),
            Err(WriteError::Failed(error)) => Err(failed(error)),
        }
    }

    /// J4: a page of the journey's history, or of the events naming `node`, from the log
    /// position `after`, as `GET /api/journeys/{id}/history` answers it.
    ///
    /// # Errors
    ///
    /// [`HostError::Missing`] when the journey does not exist; when the store fails.
    pub fn history_page(
        &self,
        journey: &JourneyId,
        node: Option<&NodeKey>,
        after: Option<u64>,
    ) -> Result<HistoryAnswer, HostError> {
        match now_or_never(self.service.history(journey, node, after)) {
            Ok(history) => Ok(HistoryAnswer {
                patches: history.patches,
                next: history.next,
            }),
            Err(cairn_service::ReadError::JourneyMissing(id)) => Err(HostError::Missing {
                message: format!("no journey {id}"),
            }),
            Err(error) => Err(failed(error)),
        }
    }

    /// The journey index (C16): every journey, in id order, up to the page limit.
    ///
    /// # Errors
    ///
    /// When the store fails.
    pub fn journey_page(&self) -> Result<JourneyPage, HostError> {
        self.journey_index(&crate::reads::JourneyIndexQuery::default())
    }
}

#[wasm_bindgen]
impl BrowserRoot {
    /// A root with every fixture seeded.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`] when seeding fails.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<BrowserRoot, String> {
        Ok(Self::seeded()?)
    }

    /// The capabilities document: one local sign-in, no assistant, MCP, or SSE.
    #[must_use]
    pub fn capabilities(&self) -> String {
        json(self.service.capabilities())
    }

    /// The journey index (C16), as `GET /api/journeys` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`].
    pub fn journeys(&self) -> Result<String, String> {
        Ok(json(&self.journey_page()?))
    }

    /// A journey's domain document at `now`, as `GET /api/journeys/{id}/document` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`]: no such journey, or an unreadable input.
    pub fn document(&self, journey: &str, now: &str) -> Result<String, String> {
        let call = self.call(now)?;
        let id: JourneyId = journey
            .parse()
            .map_err(|error| HostError::unreadable("journey", format!("{error:?}")))?;
        match now_or_never(self.service.document(&call, &id)).map_err(failed)? {
            Some(document) => Ok(json(&document)),
            None => Err(HostError::Missing {
                message: format!("no journey {id}"),
            }
            .into()),
        }
    }

    /// J4: a page of a journey's history, or of `node`'s (empty for the whole journey), after
    /// the log position `after` (negative for the first page), as `GET /api/journeys/{id}/history`
    /// answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`]: no such journey, or an unreadable input.
    pub fn history(&self, journey: &str, node: &str, after: f64) -> Result<String, String> {
        let id: JourneyId = journey
            .parse()
            .map_err(|error| HostError::unreadable("journey", format!("{error:?}")))?;
        let node: Option<NodeKey> = if node.is_empty() {
            None
        } else {
            Some(
                node.parse()
                    .map_err(|error| HostError::unreadable("node", format!("{error:?}")))?,
            )
        };
        let after = position(after)?;
        Ok(json(&self.history_page(&id, node.as_ref(), after)?))
    }

    /// A route with its draft (A11), as `GET /api/routes/{id}` answers it.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`]: no such route, or an unreadable input.
    pub fn route(&self, route: &str) -> Result<String, String> {
        let id: RouteId = route
            .parse()
            .map_err(|error| HostError::unreadable("route", format!("{error:?}")))?;
        match now_or_never(self.service.route(&id)).map_err(failed)? {
            Some(found) => Ok(json(&found)),
            None => Err(HostError::Missing {
                message: format!("no route {id}"),
            }
            .into()),
        }
    }

    /// One published version of a route (A11), as `GET /api/routes/{id}/versions/{version}`
    /// answers it; `version` is its number as text.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`]: no such route or version, or an unreadable input.
    #[wasm_bindgen(js_name = routeVersion)]
    pub fn route_version(&self, route: &str, version: &str) -> Result<String, String> {
        let id: RouteId = route
            .parse()
            .map_err(|error| HostError::unreadable("route", format!("{error:?}")))?;
        let number: VersionNumber = read("version", version)?;
        match now_or_never(self.service.route_version(&id, number)).map_err(failed)? {
            Some(found) => Ok(json(&found)),
            None => Err(HostError::Missing {
                message: format!("no version {number} of route {id}"),
            }
            .into()),
        }
    }

    /// The deployment: its entities, aliases, and revision (E6).
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`].
    pub fn deployment(&self) -> Result<String, String> {
        Ok(json(
            &now_or_never(self.service.deployment()).map_err(failed)?,
        ))
    }

    /// Submits a patch as the local user at `now`: `request` is the JSON of a
    /// [`PatchRequest`]; the answer is the JSON of a [`PatchAnswer`].
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`], the rejection among them.
    pub fn patch(&self, request: &str, now: &str) -> Result<String, String> {
        let request: PatchRequest = read("patch request", request)?;
        Ok(json(&self.submit(&request, now)?))
    }

    /// H6: a subscriber to what `watching` (the JSON of a list of watch names) names, holding
    /// the current revisions for its first take.
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`].
    pub fn subscribe(&self, watching: &str) -> Result<RootSubscription, String> {
        let names: Vec<String> = read("watching", watching)?;
        let watching = names
            .iter()
            .map(|name| watch(name))
            .collect::<Result<BTreeSet<Watch>, HostError>>()?;
        let subscription = now_or_never(self.service.subscribe(watching)).map_err(failed)?;
        Ok(RootSubscription {
            subscription,
            latest: Cell::new(Duration::ZERO),
        })
    }
}

/// One subscriber of the in-browser root (H6). Freeing it unsubscribes.
#[wasm_bindgen]
#[derive(Debug)]
pub struct RootSubscription {
    subscription: Subscription,
    /// The latest clock reading it was given: the clock must not run backwards.
    latest: Cell<Duration>,
}

impl RootSubscription {
    /// What it holds at `now` on the caller's monotonic clock.
    ///
    /// # Errors
    ///
    /// [`HostError::Unreadable`] when `now` is earlier than a reading it was given before.
    pub fn taken(&self, now: Duration) -> Result<Taken, HostError> {
        if now < self.latest.get() {
            return Err(HostError::unreadable(
                "now_ms",
                format!(
                    "{now:?} is earlier than {:?}: the clock runs backwards",
                    self.latest.get()
                ),
            ));
        }
        self.latest.set(now);
        let ticks = |ticks: Vec<cairn_store::Tick>| {
            ticks
                .into_iter()
                .map(|tick| Tick {
                    of: tick.of,
                    revision: tick.revision,
                })
                .collect()
        };
        Ok(match self.subscription.take(now) {
            Take::Current(found) => Taken::Current {
                ticks: ticks(found),
            },
            Take::Ticks(found) => Taken::Ticks {
                ticks: ticks(found),
            },
            Take::Wait { until } => Taken::Wait {
                until_ms: until.as_secs_f64() * 1000.0,
            },
            Take::Empty => Taken::Empty,
        })
    }
}

#[wasm_bindgen]
impl RootSubscription {
    /// Takes what it holds at `now_ms` (milliseconds on a monotonic clock, as
    /// `performance.now()` gives): the JSON of a [`Taken`].
    ///
    /// # Errors
    ///
    /// The JSON of a [`HostError`] when `now_ms` is not a reading of a forward-running clock:
    /// negative, not finite, too large for a duration, or earlier than one given before.
    pub fn take(&self, now_ms: f64) -> Result<String, String> {
        let now = Duration::try_from_secs_f64(now_ms / 1000.0).map_err(|error| {
            HostError::unreadable(
                "now_ms",
                format!("{now_ms} is not a clock reading: {error}"),
            )
        })?;
        Ok(json(&self.taken(now)?))
    }
}
