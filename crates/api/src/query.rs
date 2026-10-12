//! Query parameters: each endpoint's are declared once, as [`ParamSpec`]s the handler parses
//! by and the OpenAPI document describes, and a parameter an endpoint does not declare is
//! refused rather than ignored. A repeated parameter is given once per value
//! (`?status=active&status=archived`). Pages take `after` (the previous page's `next`) and
//! `size` (1 to the page limit; larger sizes are cut to it).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use cairn_schema::{
    Cursor, DisplayState, Domain, EntityKey, EventType, JourneyId, JourneyStatus, KindKey,
    LevelDisplay, LevelQuery, ListFlag, ListQuery, NextQuery, NodeKey, NodeKind, PatchId,
    ProposalId, Revision, RevisionOf, RouteId, RouteKind, SnapshotScope, SortBy, State, Timestamp,
    Title, UserId, VersionNumber,
};
use cairn_store::{EventQuery, JourneyQuery, PageSize, SearchQuery, Watch};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::parse_text;
use crate::wire::ProblemCode;

/// One query parameter an endpoint takes.
#[derive(Clone, Copy, Debug)]
pub struct ParamSpec {
    /// Its name.
    pub name: &'static str,
    /// What it does.
    pub description: &'static str,
    /// Whether it may be given more than once.
    pub repeated: bool,
    /// Whether the endpoint needs it.
    pub required: bool,
    /// The schema of one value.
    pub schema: fn(&mut SchemaGenerator) -> Schema,
}

/// The schema of `T`, as a reference to its component when it has a name.
pub fn schema_of<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<T>()
}

impl ParamSpec {
    const fn one<T: JsonSchema>(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            repeated: false,
            required: false,
            schema: schema_of::<T>,
        }
    }

    const fn many<T: JsonSchema>(name: &'static str, description: &'static str) -> Self {
        Self {
            repeated: true,
            ..Self::one::<T>(name, description)
        }
    }

    const fn needed<T: JsonSchema>(name: &'static str, description: &'static str) -> Self {
        Self {
            required: true,
            ..Self::one::<T>(name, description)
        }
    }
}

const SIZE: ParamSpec = ParamSpec::one::<u32>(
    "size",
    "Items per page, 1 to the page limit (200); larger sizes are cut to it.",
);

/// `GET /api/journeys` (C16).
pub const JOURNEY_PARAMS: &[ParamSpec] = &[
    ParamSpec::many::<JourneyStatus>("status", "Journeys in any of these statuses."),
    ParamSpec::one::<RouteId>("route", "Journeys following this route."),
    ParamSpec::one::<VersionNumber>("version", "Journeys on this version of their route."),
    ParamSpec::many::<EntityKey>(
        "referencing",
        "Journeys referring to any of these entities, directly or through an alias (E6).",
    ),
    ParamSpec::one::<bool>(
        "upgrade_available",
        "Journeys whose route has, or has not, published a newer version.",
    ),
    ParamSpec::one::<JourneyId>("after", "The page starts after this journey."),
    SIZE,
];

/// `GET /api/routes`.
pub const ROUTE_PARAMS: &[ParamSpec] = &[
    ParamSpec::one::<RouteKind>("kind", "Only routes of this kind; all kinds when absent."),
    ParamSpec::one::<RouteId>("after", "The page starts after this route."),
    SIZE,
];

/// `GET /api/search`.
pub const SEARCH_PARAMS: &[ParamSpec] = &[
    ParamSpec::needed::<Title>(
        "text",
        "What to find in journey names and descriptions, node titles and descriptions, notes, \
         and resources; ASCII letters match in either case.",
    ),
    ParamSpec::one::<JourneyId>("after", "The page starts after this journey."),
    SIZE,
];

/// `GET /api/events` (J5).
pub const EVENT_PARAMS: &[ParamSpec] = &[
    ParamSpec::one::<DomainName>("log", "Events in this domain's log."),
    ParamSpec::one::<NodeKey>("node", "Events that wrote anything on this node."),
    ParamSpec::one::<UserId>("user", "Events whose actor is this user."),
    ParamSpec::many::<EventType>("type", "Events of any of these types."),
    ParamSpec::one::<PatchId>("patch", "Events of this patch."),
    ParamSpec::one::<Timestamp>("from", "Events committed at or after this time."),
    ParamSpec::one::<Timestamp>("until", "Events committed before this time."),
    ParamSpec::one::<u64>("after", "The page starts after this position in the log."),
    SIZE,
];

/// `GET /api/events/stream` (H6).
pub const STREAM_PARAMS: &[ParamSpec] = &[ParamSpec {
    required: true,
    ..ParamSpec::many::<WatchName>(
        "domain",
        "What to watch: `deployment`, `journey:<id>`, `route:<id>`, or `proposal:<id>` for \
         one, or `journeys`, `routes`, or `proposals` for every one of a kind.",
    )
}];

const CURSOR: ParamSpec = ParamSpec::one::<Cursor>(
    "cursor",
    "Where the page starts: the previous page's `next`. A cursor is a position in the order \
     at one journey revision, so a cursor past the start needs `revision`.",
);

const REVISION: ParamSpec = ParamSpec::one::<Revision>(
    "revision",
    "The journey revision the previous page was read at (its `revision`); needed with a cursor \
     past the start. When the journey has moved since, the page is refused 409 `page_moved`: \
     start again from the first page.",
);

const KINDS: ParamSpec =
    ParamSpec::many::<NodeKind>("kind", "Only nodes of these kinds; every kind when none.");

const SORT: ParamSpec = ParamSpec::one::<SortBy>("sort", "The signal to sort by; rank when none.");

/// `GET /api/journeys/{id}/snapshot` (I3).
pub const SNAPSHOT_PARAMS: &[ParamSpec] = &[
    ParamSpec::one::<NodeKey>(
        "subtree",
        "This node and everything beneath it; the whole journey when none.",
    ),
    ParamSpec::one::<u32>(
        "depth",
        "How many levels below the subtree's node (or of roots, at 1) the node list goes; every \
         level when none.",
    ),
    CURSOR,
    REVISION,
];

/// `GET /api/journeys/{id}/level` (C2).
pub const LEVEL_PARAMS: &[ParamSpec] = &[
    ParamSpec::many::<NodeKind>("kind", "The kinds shown; every kind when none."),
    ParamSpec::one::<NodeKey>(
        "container",
        "The container drilled into; the top level when none.",
    ),
    ParamSpec::many::<NodeKey>(
        "collapsed",
        "Containers rolled into their own cards: everything beneath each is hidden and rolls up          into it, and its edges re-target to it. A key outside the level has no effect.",
    ),
    ParamSpec::many::<LevelDisplay>(
        "display",
        "The relevance classes shown: `relevant`, `conditional` (undecided, or waiting on a          decision that cannot be answered yet), `not_relevant` (settled). Every class when none.          A node of a class left out rolls up like a hidden kind's.",
    ),
];

/// `GET /api/journeys/{id}/next` (C10).
pub const NEXT_PARAMS: &[ParamSpec] = &[
    SORT,
    ParamSpec::one::<bool>("mine", "Only nodes the caller participates in (E4)."),
    KINDS,
    ParamSpec::one::<NodeKey>("within", "Only this node and the nodes beneath it."),
    ParamSpec::one::<bool>(
        "for_viewer",
        "Rank for the caller: prioritize for me (Priority).",
    ),
    ParamSpec::one::<u32>(
        "top",
        "Only the first this many items; every item when none. The stalled diagnostic is \
         unaffected.",
    ),
];

/// `GET /api/journeys/{id}/nodes` (C9).
pub const LIST_PARAMS: &[ParamSpec] = &[
    ParamSpec::many::<ListFlag>(
        "flag",
        "Filters with no argument; every one given must hold.",
    ),
    ParamSpec::one::<NodeKey>("within", "Only nodes beneath this container."),
    ParamSpec::one::<EntityKey>("owner", "Only nodes this entity owns."),
    ParamSpec::many::<State>("state", "Only nodes in these stored states; any when none."),
    ParamSpec::many::<DisplayState>(
        "display_state",
        "Only nodes showing these display states; any when none.",
    ),
    KINDS,
    ParamSpec::one::<Title>(
        "text",
        "Text found in the title, description, notes, or resources, ignoring ASCII case.",
    ),
    SORT,
    CURSOR,
    REVISION,
];

/// `GET /api/journeys/{id}/mine` (E4).
pub const MINE_PARAMS: &[ParamSpec] = &[ParamSpec::many::<KindKey>(
    "kind",
    "Only these participation kinds; every kind when none.",
)];

/// `GET /api/journeys/{id}/nodes/{key}/explanations/{field}`.
pub const EXPLANATION_PARAMS: &[ParamSpec] = &[CURSOR, REVISION];

/// `GET /api/journeys/{id}/history` (J4).
pub const HISTORY_PARAMS: &[ParamSpec] = &[
    ParamSpec::one::<NodeKey>("node", "Only the events that wrote anything on this node."),
    ParamSpec::one::<u64>("after", "The page starts after this position in the log."),
];

/// `GET /api/routes/{id}/export` (A13).
pub const EXPORT_PARAMS: &[ParamSpec] = &[ParamSpec::one::<VersionNumber>(
    "version",
    "The published version to export; the draft when none.",
)];

/// A request's query parameters, checked against what its endpoint declares.
#[derive(Debug)]
pub struct Params {
    pairs: Vec<(String, String)>,
}

impl Params {
    /// Parses `query`, refusing a parameter not in `specs`, one given twice that is not
    /// repeated, and a missing required one.
    ///
    /// # Errors
    ///
    /// A bad request naming the parameter.
    pub fn parse(query: Option<&str>, specs: &[ParamSpec]) -> Result<Self, ApiError> {
        let pairs: Vec<(String, String)> =
            url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect();
        for (name, _) in &pairs {
            if !specs.iter().any(|spec| spec.name == name) {
                return Err(ApiError::bad_request(format!(
                    "no query parameter {name:?} here"
                )));
            }
        }
        for spec in specs {
            let given = pairs.iter().filter(|(name, _)| name == spec.name).count();
            if given > 1 && !spec.repeated {
                return Err(ApiError::bad_request(format!(
                    "{} is given more than once",
                    spec.name
                )));
            }
            if given == 0 && spec.required {
                return Err(ApiError::bad_request(format!("{} is required", spec.name)));
            }
        }
        Ok(Self { pairs })
    }

    /// Every value of `name`, in order.
    ///
    /// # Errors
    ///
    /// A bad request naming the parameter and the value that does not parse.
    pub fn all<T: DeserializeOwned>(&self, name: &str) -> Result<Vec<T>, ApiError> {
        let values = self.pairs.iter().filter(|(given, _)| given == name);
        values
            .map(|(_, value)| {
                parse_text(value)
                    .map_err(|reason| ApiError::bad_request(format!("{name}={value:?}: {reason}")))
            })
            .collect()
    }

    /// The value of `name`, if given.
    ///
    /// # Errors
    ///
    /// A bad request when it does not parse.
    pub fn one<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, ApiError> {
        Ok(self.all(name)?.into_iter().next())
    }

    /// Where a projection's page starts: `cursor`, or the first item.
    ///
    /// # Errors
    ///
    /// A bad request when it does not parse.
    pub fn cursor(&self) -> Result<Cursor, ApiError> {
        Ok(self.one("cursor")?.unwrap_or(Cursor::START))
    }

    /// I3, C9: a page past the start continues the order at the revision its cursor came
    /// from, so the projection's revision `at` must be the `revision` given with it; a first
    /// page needs none, and checks one given.
    ///
    /// # Errors
    ///
    /// A bad request when a cursor past the start comes without a revision, or either does not
    /// parse; `page_moved` when the journey is no longer at that revision.
    pub fn page_at(&self, at: Revision) -> Result<(), ApiError> {
        let read_at: Option<Revision> = self.one("revision")?;
        match read_at {
            None if self.cursor()? != Cursor::START => Err(ApiError::bad_request(
                "a cursor past the start needs the revision of the page it came from",
            )),
            Some(read_at) if read_at != at => Err(ApiError::problem(
                ProblemCode::PageMoved,
                format!(
                    "the journey moved from revision {read_at} to {at} since that page was \
                     read; start again from the first page"
                ),
            )),
            None | Some(_) => Ok(()),
        }
    }

    fn size(&self) -> Result<PageSize, ApiError> {
        Ok(self
            .one::<u32>("size")?
            .map_or(PageSize::MAX, PageSize::new))
    }
}

/// C16: the journey index query.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn journeys(params: &Params) -> Result<JourneyQuery, ApiError> {
    let referencing: Vec<EntityKey> = params.all("referencing")?;
    Ok(JourneyQuery {
        statuses: params.all("status")?.into_iter().collect(),
        route: params.one("route")?,
        version: params.one("version")?,
        referencing: (!referencing.is_empty()).then(|| referencing.into_iter().collect()),
        upgrade_available: params.one("upgrade_available")?,
        after: params.one("after")?,
        size: params.size()?,
    })
}

/// The route index's page: the kind it lists, where it starts, and its size.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn routes(params: &Params) -> Result<(Option<RouteKind>, Option<RouteId>, PageSize), ApiError> {
    Ok((params.one("kind")?, params.one("after")?, params.size()?))
}

/// The search query.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn search(params: &Params) -> Result<SearchQuery, ApiError> {
    let text: Option<Title> = params.one("text")?;
    let Some(text) = text else {
        return Err(ApiError::bad_request("text is required"));
    };
    Ok(SearchQuery {
        text,
        after: params.one("after")?,
        size: params.size()?,
    })
}

/// J5: the event query.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn events(params: &Params) -> Result<EventQuery, ApiError> {
    let log: Option<DomainName> = params.one("log")?;
    Ok(EventQuery {
        log: log.map(|name| name.0),
        node: params.one("node")?,
        user: params.one("user")?,
        types: params.all("type")?.into_iter().collect(),
        patch: params.one("patch")?,
        from: params.one("from")?,
        until: params.one("until")?,
        after: params.one("after")?,
        size: params.size()?,
    })
}

/// I3: the snapshot's scope.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn snapshot(params: &Params) -> Result<SnapshotScope, ApiError> {
    Ok(SnapshotScope {
        subtree: params.one("subtree")?,
        depth: params.one("depth")?,
        cursor: params.cursor()?,
    })
}

/// C2: a level's query: the kinds shown (every kind when none is given), the container
/// drilled into, the containers collapsed, and the relevance classes shown (every class when
/// none is given).
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn level(params: &Params) -> Result<LevelQuery, ApiError> {
    Ok(LevelQuery::requested(
        params.all("kind")?.into_iter().collect(),
        params.one("container")?,
        params.all("collapsed")?.into_iter().collect(),
        params.all("display")?.into_iter().collect(),
    ))
}

/// C10: how many of the next list's items to answer with; every item when none.
///
/// # Errors
///
/// A bad request when it does not parse.
pub fn next_top(params: &Params) -> Result<Option<usize>, ApiError> {
    let top: Option<u32> = params.one("top")?;
    Ok(top.map(|top| top as usize))
}

/// C10: the next list's query.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn next(params: &Params) -> Result<NextQuery, ApiError> {
    Ok(NextQuery {
        sort: params.one("sort")?.unwrap_or_default(),
        mine: params.one("mine")?.unwrap_or(false),
        kinds: params.all("kind")?.into_iter().collect(),
        within: params.one("within")?,
        for_viewer: params.one("for_viewer")?.unwrap_or(false),
    })
}

/// C9: the list's query.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn list(params: &Params) -> Result<ListQuery, ApiError> {
    Ok(ListQuery {
        flags: params.all("flag")?.into_iter().collect(),
        within: params.one("within")?,
        owner: params.one("owner")?,
        states: params.all("state")?.into_iter().collect(),
        display_states: params.all("display_state")?.into_iter().collect(),
        kinds: params.all("kind")?.into_iter().collect(),
        text: params.one("text")?,
        sort: params.one("sort")?.unwrap_or_default(),
        cursor: params.cursor()?,
    })
}

/// E4: the participation kinds asked for; every kind when empty.
///
/// # Errors
///
/// A bad request for a value that does not parse.
pub fn mine(params: &Params) -> Result<BTreeSet<KindKey>, ApiError> {
    Ok(params.all("kind")?.into_iter().collect())
}

/// J4: the node whose history is asked for, and where the page starts.
///
/// # Errors
///
/// A bad request for a parameter that does not parse.
pub fn history(params: &Params) -> Result<(Option<NodeKey>, Option<u64>), ApiError> {
    Ok((params.one("node")?, params.one("after")?))
}

/// H6: what a stream watches.
///
/// # Errors
///
/// A bad request for a value that does not parse.
pub fn watching(params: &Params) -> Result<BTreeSet<Watch>, ApiError> {
    let names: Vec<WatchName> = params.all("domain")?;
    Ok(names.into_iter().map(|name| name.0).collect())
}

/// A domain named in a query: `deployment`, `journey:<id>`, or `route:<id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainName(pub Domain);

impl FromStr for DomainName {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let domain = match text.split_once(':') {
            None if text == "deployment" => Domain::Deployment,
            Some(("journey", id)) => {
                Domain::Journey(id.parse().map_err(|error| format!("{error}"))?)
            }
            Some(("route", id)) => Domain::Route(id.parse().map_err(|error| format!("{error}"))?),
            _ => {
                return Err(format!(
                    "{text:?} is not deployment, journey:<id>, or route:<id>"
                ));
            }
        };
        Ok(Self(domain))
    }
}

impl fmt::Display for DomainName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Domain::Deployment => formatter.write_str("deployment"),
            Domain::Journey(id) => write!(formatter, "journey:{id}"),
            Domain::Route(id) => write!(formatter, "route:{id}"),
        }
    }
}

/// What a stream watches, as a query names it: one domain or proposal (`deployment`,
/// `journey:<id>`, `route:<id>`, `proposal:<id>`), or every one of a kind (`journeys`,
/// `routes`, `proposals`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchName(pub Watch);

impl FromStr for WatchName {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let watch = match text {
            "journeys" => Watch::Journeys,
            "routes" => Watch::Routes,
            "proposals" => Watch::Proposals,
            _ => match text.split_once(':') {
                Some(("proposal", id)) => {
                    let id: ProposalId = id.parse().map_err(|error| format!("{error}"))?;
                    Watch::One(RevisionOf::Proposal(id))
                }
                _ => Watch::One(RevisionOf::Domain(text.parse::<DomainName>()?.0)),
            },
        };
        Ok(Self(watch))
    }
}

impl fmt::Display for WatchName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Watch::Journeys => formatter.write_str("journeys"),
            Watch::Routes => formatter.write_str("routes"),
            Watch::Proposals => formatter.write_str("proposals"),
            Watch::One(RevisionOf::Proposal(id)) => write!(formatter, "proposal:{id}"),
            Watch::One(RevisionOf::Domain(domain)) => DomainName(domain.clone()).fmt(formatter),
        }
    }
}

/// Serde and JSON Schema for a name written as a string with a pattern.
macro_rules! named_in_text {
    ($type:ty, $name:literal, $description:literal, $pattern:literal) => {
        impl Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $type {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                schemars::json_schema!({
                    "type": "string",
                    "description": $description,
                    "pattern": $pattern,
                })
            }
        }
    };
}

named_in_text!(
    DomainName,
    "DomainName",
    "A domain: `deployment`, `journey:<id>`, or `route:<id>`.",
    "^(deployment|journey:.+|route:.+)$"
);
named_in_text!(
    WatchName,
    "WatchName",
    "What a stream watches: `deployment`, `journey:<id>`, `route:<id>`, `proposal:<id>`, \
     `journeys`, `routes`, or `proposals`.",
    "^(deployment|journeys|routes|proposals|(journey|route|proposal):.+)$"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_through_text() {
        for text in [
            "deployment",
            "journey:j_one",
            "route:r_one",
            "proposal:pr_one",
            "journeys",
            "routes",
            "proposals",
        ] {
            let name: WatchName = text.parse().unwrap();
            assert_eq!(name.to_string(), text);
        }
        for text in ["journey", "node:n_a", "journey:", ""] {
            assert!(text.parse::<WatchName>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn parameters_are_checked_against_what_the_endpoint_declares() {
        let refused = [
            "colour=blue",
            "route=r_a&route=r_b",
            "version=two",
            "status=paused",
        ];
        for query in refused {
            let checked = Params::parse(Some(query), JOURNEY_PARAMS).and_then(|p| journeys(&p));
            assert!(checked.is_err(), "{query}");
        }
        let params = Params::parse(
            Some("status=active&status=archived&size=500"),
            JOURNEY_PARAMS,
        )
        .unwrap();
        let query = journeys(&params).unwrap();
        assert_eq!(query.statuses.len(), 2);
        assert_eq!(query.size, PageSize::MAX);
        assert!(
            Params::parse(None, SEARCH_PARAMS).is_err(),
            "text is required"
        );
    }
}
