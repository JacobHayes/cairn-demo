//! What the store's queries take and return: the current revisions (H6), the journey index
//! (C16), route detail (C17), the journeys referencing an entity (E6), event history (J5),
//! and text search across journeys.

use std::collections::{BTreeMap, BTreeSet};

use cairn_schema::{
    AttachmentKey, Domain, EntityKey, Event, EventType, GraphId, InsertionKey, JourneyId,
    JourneyStatus, Lineage, NodeKey, PatchId, ProposalId, Revision, RouteHeader, RouteId,
    Timestamp, Title, UserId, VersionNumber,
};

use crate::limits::PAGE_ITEM_COUNT_MAX;

/// The current revision of every domain and proposal: what a new subscriber starts from, so
/// a commit between a fetch and a subscription is never missed (ARCHITECTURE, Concurrency
/// and notification; H6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Revisions {
    /// The deployment revision.
    pub deployment: Revision,
    /// Every journey's revision.
    pub journeys: BTreeMap<JourneyId, Revision>,
    /// Every route's revision.
    pub routes: BTreeMap<RouteId, Revision>,
    /// Every proposal's destination and editing revision.
    pub proposals: BTreeMap<ProposalId, (Domain, Revision)>,
}

/// How many items a page holds: 1 to `PAGE_ITEM_COUNT_MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageSize(u32);

impl PageSize {
    /// The largest page.
    pub const MAX: PageSize = PageSize(PAGE_ITEM_COUNT_MAX);

    /// A page of `items`, at least 1 and at most the limit; other sizes are clamped.
    #[must_use]
    pub fn new(items: u32) -> Self {
        Self(items.clamp(1, PAGE_ITEM_COUNT_MAX))
    }

    /// The number of items.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }

    /// The number of items, as a length.
    #[must_use]
    pub fn len(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }

    /// Never: a page holds at least one item.
    #[must_use]
    pub fn is_empty(self) -> bool {
        false
    }
}

impl Default for PageSize {
    fn default() -> Self {
        Self::MAX
    }
}

/// One page of results, in order, and the cursor the next page starts after; none on the
/// last page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T, C> {
    /// The items.
    pub items: Vec<T>,
    /// Where the next page starts, if there is one.
    pub next: Option<C>,
}

impl<T, C> Page<T, C> {
    /// Cuts `items` (in order, possibly one past the page) to `size`, with the cursor of the
    /// last item kept when more remain.
    pub fn cut(mut items: Vec<T>, size: PageSize, cursor: impl Fn(&T) -> C) -> Self {
        let more = items.len() > size.len();
        items.truncate(size.len());
        let next = if more { items.last().map(cursor) } else { None };
        Self { items, next }
    }
}

/// C16: the journey index's filters. Every filter given must hold. "Mine" is derived (D3),
/// so the store narrows to the journeys that refer to the viewer's entities at all, and the
/// service derives the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JourneyQuery {
    /// Journeys in any of these statuses; empty for any status.
    pub statuses: BTreeSet<JourneyStatus>,
    /// Journeys following this route.
    pub route: Option<RouteId>,
    /// Journeys on this version of their route.
    pub version: Option<VersionNumber>,
    /// Journeys referring to any of these entities, directly or through an alias (E6).
    pub referencing: Option<BTreeSet<EntityKey>>,
    /// Journeys whose route has, or has not, published a version newer than theirs.
    pub upgrade_available: Option<bool>,
    /// The page starts after this journey.
    pub after: Option<JourneyId>,
    /// The page's size.
    pub size: PageSize,
}

/// A journey in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JourneySummary {
    /// The journey.
    pub id: JourneyId,
    /// Its name.
    pub name: Title,
    /// Its status.
    pub status: JourneyStatus,
    /// The route version it follows.
    pub lineage: Option<Lineage>,
    /// Its revision.
    pub revision: Revision,
    /// When it was created.
    pub created_at: Timestamp,
    /// The latest version its route has published.
    pub latest_version: Option<VersionNumber>,
    /// The number of nodes in the latest published route version.
    pub latest_version_node_count: Option<u32>,
}

impl JourneySummary {
    /// C16: whether the journey's route has published a version newer than the journey's.
    #[must_use]
    pub fn upgrade_available(&self) -> bool {
        match (&self.lineage, self.latest_version) {
            (Some(lineage), Some(latest)) => latest > lineage.version,
            _ => false,
        }
    }
}

/// C17: a route's published versions with the journeys on each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteDetail {
    /// The route's fields.
    pub header: RouteHeader,
    /// Its revision.
    pub revision: Revision,
    /// Its versions, oldest first.
    pub versions: Vec<VersionJourneys>,
}

/// One published version and the journeys on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionJourneys {
    /// The version.
    pub version: VersionNumber,
    /// When it was published.
    pub published_at: Timestamp,
    /// The journeys whose lineage is this version.
    pub journeys: BTreeSet<JourneyId>,
    /// The insertions of this version, when the route is a segment (C19).
    pub insertions: Vec<InsertionUse>,
}

/// One insertion of a segment version, as the segment's detail lists it (C19).
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// J5: event filters. Every filter given must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventQuery {
    /// Events in this domain's log.
    pub log: Option<Domain>,
    /// Events that wrote anything on this node, or are about it.
    pub node: Option<NodeKey>,
    /// Events whose actor is this user.
    pub user: Option<UserId>,
    /// Events of any of these types; empty for any type.
    pub types: BTreeSet<EventType>,
    /// Events of this patch.
    pub patch: Option<PatchId>,
    /// Events committed at or after this time.
    pub from: Option<Timestamp>,
    /// Events committed before this time.
    pub until: Option<Timestamp>,
    /// The page starts after this position in the log.
    pub after: Option<u64>,
    /// The page's size.
    pub size: PageSize,
}

/// An event and its position in the store's log, which orders history and pages it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedEvent {
    /// The position: increasing in commit order.
    pub seq: u64,
    /// The event.
    pub event: Event,
}

/// Text search across journeys: names, descriptions, node titles and descriptions, notes,
/// and resources, matched as a substring with ASCII letters compared case-insensitively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    /// What to find.
    pub text: Title,
    /// The page starts after this journey.
    pub after: Option<JourneyId>,
    /// The page's size, in journeys.
    pub size: PageSize,
}

/// One journey's matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JourneyMatches {
    /// The journey.
    pub journey: JourneyId,
    /// Its name.
    pub name: Title,
    /// Where the text was found, in order.
    pub hits: BTreeSet<SearchHit>,
}

/// Where search text was found in a journey.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SearchHit {
    /// The journey's name.
    JourneyName,
    /// The journey's description.
    JourneyDescription,
    /// A node's title.
    NodeTitle(NodeKey),
    /// A node's description.
    NodeDescription(NodeKey),
    /// A note or link: its title or content.
    Annotation(AttachmentKey),
    /// A resource on a node: its title or content.
    Resource {
        /// The node.
        node: NodeKey,
        /// The resource.
        resource: AttachmentKey,
    },
}

/// Whether `haystack` holds `needle`, comparing ASCII letters case-insensitively: the
/// search every backend runs.
#[must_use]
pub fn text_matches(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_keeps_the_cursor_only_when_more_remain() {
        let size = PageSize::new(2);
        let full = Page::cut(vec![1, 2, 3], size, |item| *item);
        assert_eq!((full.items, full.next), (vec![1, 2], Some(2)));
        let last = Page::cut(vec![1, 2], size, |item| *item);
        assert_eq!((last.items, last.next), (vec![1, 2], None));
    }

    #[test]
    fn page_sizes_stay_within_the_limit() {
        assert_eq!(PageSize::new(0).get(), 1);
        assert_eq!(PageSize::new(u32::MAX), PageSize::MAX);
        assert_eq!(PageSize::default().get(), PAGE_ITEM_COUNT_MAX);
    }

    #[test]
    fn text_matches_folds_ascii_case_only() {
        let cases = [
            ("Vendor Review", "vendor", true),
            ("vendor review", "REVIEW", true),
            ("Ünit", "ünit", false),
            ("Ünit", "Ünit", true),
            ("50% done", "% d", true),
            ("abc", "abd", false),
        ];
        for (haystack, needle, expected) in cases {
            assert_eq!(
                text_matches(haystack, needle),
                expected,
                "{haystack:?} {needle:?}"
            );
        }
    }
}
