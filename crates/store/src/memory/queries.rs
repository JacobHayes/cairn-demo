//! The memory backend's index and history queries: scans over the records, the reference
//! answers every backend's queries are held to.

use std::collections::{BTreeMap, BTreeSet};

use cairn_schema::{Entity, EntityKey, GraphId, JourneyId, RouteId, VersionNumber};

use super::State;
use crate::backend;
use crate::query::{
    EventQuery, JourneyMatches, JourneyQuery, JourneySummary, LoggedEvent, Page, RouteDetail,
    SearchHit, SearchQuery, VersionJourneys, text_matches,
};

impl State {
    fn latest_version(&self, route: &RouteId) -> Option<VersionNumber> {
        self.domains
            .versions
            .keys()
            .filter(|(held, _)| held == route)
            .map(|(_, version)| *version)
            .max()
    }

    pub(super) fn journeys(&self, query: &JourneyQuery) -> Page<JourneySummary, JourneyId> {
        let referencing = query
            .referencing
            .as_ref()
            .map(|entities| self.domains.journeys_referencing(entities));
        let mut items = Vec::new();
        for (id, (header, revision)) in &self.domains.journeys {
            let summary = JourneySummary {
                id: id.clone(),
                name: header.name.clone(),
                status: header.status,
                lineage: header.lineage.clone(),
                revision: *revision,
                created_at: header.created_at,
                latest_version: header
                    .lineage
                    .as_ref()
                    .and_then(|lineage| self.latest_version(&lineage.route)),
                latest_version_node_count: header.lineage.as_ref().and_then(|lineage| {
                    let version = self.latest_version(&lineage.route)?;
                    let graph = self.domains.graphs.get(&GraphId::RouteVersion {
                        route: lineage.route.clone(),
                        version,
                    });
                    Some(graph.map_or(0, |graph| {
                        u32::try_from(graph.nodes.len()).unwrap_or(u32::MAX)
                    }))
                }),
            };
            if backend::journey_listed(query, &summary, referencing.as_ref()) {
                items.push(summary);
                if items.len() > query.size.len() {
                    break;
                }
            }
        }
        Page::cut(items, query.size, |summary| summary.id.clone())
    }

    pub(super) fn route_detail(&self, route: &RouteId) -> Option<RouteDetail> {
        let (header, revision) = self.domains.routes.get(route)?;
        let mut uses = backend::insertion_uses(
            route,
            |held| self.latest_version(held),
            |version| {
                let id = GraphId::RouteVersion {
                    route: route.clone(),
                    version,
                };
                let root = self.domains.graphs.get(&id)?.nodes.values();
                root.into_iter()
                    .find(|node| node.parent.is_none())
                    .map(|node| node.key.clone())
            },
            self.domains.graphs.iter(),
        );
        let versions = self
            .domains
            .versions
            .iter()
            .filter(|((held, _), _)| held == route)
            .map(|((_, version), published_at)| VersionJourneys {
                insertions: uses.remove(version).unwrap_or_default(),
                version: *version,
                published_at: *published_at,
                journeys: self
                    .domains
                    .journeys
                    .iter()
                    .filter(|(_, (header, _))| {
                        header.lineage.as_ref().is_some_and(|lineage| {
                            lineage.route == *route && lineage.version == *version
                        })
                    })
                    .map(|(id, _)| id.clone())
                    .collect(),
            })
            .collect();
        Some(RouteDetail {
            header: header.clone(),
            revision: *revision,
            versions,
        })
    }

    pub(super) fn resolve_entity(&self, key: &EntityKey) -> Option<Entity> {
        let deployment = &self.domains.deployment;
        let key = deployment.aliases.get(key).unwrap_or(key);
        deployment.entities.get(key).cloned()
    }

    pub(super) fn events(&self, query: &EventQuery) -> Page<LoggedEvent, u64> {
        let mut items = Vec::new();
        for logged in &self.events {
            let event = &logged.event;
            let keep = query.after.is_none_or(|after| logged.seq > after)
                && query.log.as_ref().is_none_or(|log| event.log == *log)
                && query
                    .user
                    .as_ref()
                    .is_none_or(|user| event.actor.user == *user)
                && (query.types.is_empty() || query.types.contains(&event.event_type))
                && query
                    .patch
                    .as_ref()
                    .is_none_or(|patch| event.patch_id == *patch)
                && query.from.is_none_or(|from| event.at >= from)
                && query.until.is_none_or(|until| event.at < until)
                && query.node.as_ref().is_none_or(|node| {
                    self.event_nodes
                        .get(&logged.seq)
                        .is_some_and(|nodes| nodes.contains(node))
                });
            if keep {
                items.push(logged.clone());
                if items.len() > query.size.len() {
                    break;
                }
            }
        }
        Page::cut(items, query.size, |logged| logged.seq)
    }

    pub(super) fn search(&self, query: &SearchQuery) -> Page<JourneyMatches, JourneyId> {
        let needle = query.text.as_str();
        let mut items = Vec::new();
        for (id, (header, _)) in &self.domains.journeys {
            if query.after.as_ref().is_some_and(|after| id <= after) {
                continue;
            }
            let mut hits = BTreeSet::new();
            if text_matches(header.name.as_str(), needle) {
                hits.insert(SearchHit::JourneyName);
            }
            if let Some(description) = &header.description
                && text_matches(description.as_str(), needle)
            {
                hits.insert(SearchHit::JourneyDescription);
            }
            hits.extend(graph_hits(
                &self.domains.graph(&GraphId::Journey(id.clone())),
                needle,
            ));
            if !hits.is_empty() {
                items.push(JourneyMatches {
                    journey: id.clone(),
                    name: header.name.clone(),
                    hits,
                });
                if items.len() > query.size.len() {
                    break;
                }
            }
        }
        Page::cut(items, query.size, |matches| matches.journey.clone())
    }
}

/// Where `needle` appears in a journey's graph.
fn graph_hits(graph: &cairn_schema::Graph, needle: &str) -> BTreeSet<SearchHit> {
    let mut hits = BTreeSet::new();
    for node in graph.nodes.values() {
        if text_matches(node.title.as_str(), needle) {
            hits.insert(SearchHit::NodeTitle(node.key.clone()));
        }
        if let Some(description) = &node.description
            && text_matches(description.as_str(), needle)
        {
            hits.insert(SearchHit::NodeDescription(node.key.clone()));
        }
        for resource in &node.resources {
            if text_matches(&backend::resource_text(resource), needle) {
                hits.insert(SearchHit::Resource {
                    node: node.key.clone(),
                    resource: resource.key.clone(),
                });
            }
        }
    }
    for annotation in graph.state.annotations.values() {
        if text_matches(&backend::annotation_text(annotation), needle) {
            hits.insert(SearchHit::Annotation(annotation.body.key.clone()));
        }
    }
    hits
}

/// The journeys referencing entities, for the trait's query.
pub(super) fn referencing(
    state: &State,
    entities: &BTreeSet<EntityKey>,
) -> BTreeMap<JourneyId, cairn_schema::Revision> {
    state.domains.journeys_referencing(entities)
}
