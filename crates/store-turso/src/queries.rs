//! Index and history queries (C16, C17, E6, J5, text search), answering as the memory
//! backend does (`cairn_store::memory`).

use std::collections::{BTreeMap, BTreeSet};

use cairn_schema::{Actor, Entity, EntityKey, Event, JourneyId, Revision, RouteId};
use cairn_store::backend;
use cairn_store::{
    EventQuery, JourneyMatches, JourneyQuery, JourneySummary, LoggedEvent, Page, RouteDetail,
    SearchHit, SearchQuery, StoreError, VersionJourneys,
};
use turso::{Connection, Value};

use crate::load;
use crate::sql::{corrupt, domain_columns, first, int, json_enum, parse, rows, text, time};

const JOURNEY_GRAPH: &str = "journey/";

fn journey_of_graph(graph: &str) -> Result<Option<JourneyId>, StoreError> {
    graph.strip_prefix(JOURNEY_GRAPH).map(parse).transpose()
}

pub(crate) async fn journeys(
    connection: &Connection,
    query: &JourneyQuery,
) -> Result<Page<JourneySummary, JourneyId>, StoreError> {
    let referencing = match &query.referencing {
        Some(entities) => Some(referencing(connection, entities).await?),
        None => None,
    };
    let select = "SELECT j.id, j.name, j.status, j.lineage_route, j.lineage_version, j.revision, \
                  j.created_at, (SELECT max(v.version_number) FROM route_versions v \
                  WHERE v.route_id = j.lineage_route) \
                  FROM journeys j WHERE ?1 IS NULL OR j.id > ?1 ORDER BY j.id";
    let after = query.after.as_ref().map_or(Value::Null, text);
    let mut items = Vec::new();
    for row in rows(connection, select, vec![after]).await? {
        let summary = JourneySummary {
            id: row.parse(0)?,
            name: row.parse(1)?,
            status: row.name(2)?,
            lineage: load::lineage(&row, 3)?,
            revision: row.number(5)?,
            created_at: row.timestamp(6)?,
            latest_version: row.opt_int(7)?.map(crate::sql::number).transpose()?,
            latest_version_node_count: None,
        };
        if backend::journey_listed(query, &summary, referencing.as_ref()) {
            items.push(summary);
            if items.len() > query.size.len() {
                break;
            }
        }
    }
    let mut page = Page::cut(items, query.size, |summary| summary.id.clone());
    // C16: refresh publication metadata alongside the journey index in a single batch query.
    let route_ids: Vec<&RouteId> = page
        .items
        .iter()
        .filter_map(|j| j.lineage.as_ref().map(|l| &l.route))
        .collect();
    let summaries = publication_summaries(connection, &route_ids).await?;
    for journey in &mut page.items {
        if let Some(lineage) = &journey.lineage {
            if let Some((version, node_count)) = summaries.get(&lineage.route) {
                journey.latest_version = *version;
                journey.latest_version_node_count = *node_count;
            }
        }
    }
    Ok(page)
}

/// C16: the latest publication and its node count for a batch of routes, in a single query.
async fn publication_summaries(
    connection: &Connection,
    route_ids: &[&RouteId],
) -> Result<BTreeMap<RouteId, (Option<cairn_schema::VersionNumber>, Option<u32>)>, StoreError> {
    if route_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = (1..=route_ids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let select = format!(
        "WITH latest AS (\
           SELECT route_id, max(version_number) AS version_number \
           FROM route_versions WHERE route_id IN ({placeholders}) GROUP BY route_id\
         ) \
         SELECT l.route_id, l.version_number, count(n.graph_id) \
         FROM latest l \
         LEFT JOIN nodes n ON n.graph_id IN (\
           SELECT g.id FROM graphs g \
           WHERE g.route_id = l.route_id \
           AND g.version_number = l.version_number \
           AND g.kind = 'route_version'\
         ) \
         GROUP BY l.route_id, l.version_number"
    );
    let params: Vec<Value> = route_ids.iter().map(|r| text(*r)).collect();
    let mut result = BTreeMap::new();
    for row in rows(connection, &select, params).await? {
        let route: RouteId = row.parse(0)?;
        let version = row.number(1)?;
        let node_count = row.int(2)? as u32;
        result.insert(route, (Some(version), Some(node_count)));
    }
    Ok(result)
}

pub(crate) async fn route_detail(
    connection: &Connection,
    route: &RouteId,
) -> Result<Option<RouteDetail>, StoreError> {
    let Some((header, revision)) = load::route_row(connection, route).await? else {
        return Ok(None);
    };
    let latest = latest_versions(connection).await?;
    let select = "SELECT DISTINCT graph_id FROM insertions WHERE segment = ?1";
    let mut graphs = Vec::new();
    for row in rows(connection, select, vec![text(route)]).await? {
        let id = crate::sql::graph_from(&row.text(0)?)?;
        graphs.push((id.clone(), crate::graph::load(connection, &id).await?));
    }
    let roots = version_roots(connection, route).await?;
    let mut uses = backend::insertion_uses(
        route,
        |held| latest.get(held).copied(),
        |version| roots.get(&version).cloned(),
        graphs.iter().map(|(id, graph)| (id, graph)),
    );
    let select = "SELECT version_number, published_at FROM route_versions WHERE route_id = ?1 \
                  ORDER BY version_number";
    let mut versions = Vec::new();
    for row in rows(connection, select, vec![text(route)]).await? {
        let version: cairn_schema::VersionNumber = row.number(0)?;
        let select = "SELECT id FROM journeys WHERE lineage_route = ?1 AND lineage_version = ?2";
        let mut journeys = BTreeSet::new();
        for journey in rows(connection, select, vec![text(route), int(version.get())]).await? {
            journeys.insert(journey.parse(0)?);
        }
        versions.push(VersionJourneys {
            version,
            published_at: row.timestamp(1)?,
            journeys,
            insertions: uses.remove(&version).unwrap_or_default(),
        });
    }
    Ok(Some(RouteDetail {
        header,
        revision,
        versions,
    }))
}

/// The key of each published version's root of `route`.
async fn version_roots(
    connection: &Connection,
    route: &RouteId,
) -> Result<BTreeMap<cairn_schema::VersionNumber, cairn_schema::NodeKey>, StoreError> {
    let select = "SELECT graph_id, key FROM nodes WHERE parent_key IS NULL AND graph_id LIKE ?1";
    let pattern = format!("version/{route}/%");
    let mut roots = BTreeMap::new();
    for row in rows(connection, select, vec![text(&pattern)]).await? {
        // `_` in a route's id matches any character in LIKE, so the graph is checked exactly.
        if let cairn_schema::GraphId::RouteVersion {
            route: held,
            version,
        } = crate::sql::graph_from(&row.text(0)?)?
            && held == *route
        {
            roots.insert(version, row.parse(1)?);
        }
    }
    Ok(roots)
}

/// Each route's latest published version.
async fn latest_versions(
    connection: &Connection,
) -> Result<BTreeMap<RouteId, cairn_schema::VersionNumber>, StoreError> {
    let select = "SELECT route_id, max(version_number) FROM route_versions GROUP BY route_id";
    let mut latest = BTreeMap::new();
    for row in rows(connection, select, Vec::new()).await? {
        latest.insert(row.parse(0)?, row.number(1)?);
    }
    Ok(latest)
}

/// E6: the journeys referring to any of `entities`, directly or through an alias.
pub(crate) async fn referencing(
    connection: &Connection,
    entities: &BTreeSet<EntityKey>,
) -> Result<BTreeMap<JourneyId, Revision>, StoreError> {
    let mut aliases = BTreeMap::new();
    for row in rows(
        connection,
        "SELECT alias, entity FROM entity_aliases",
        Vec::new(),
    )
    .await?
    {
        aliases.insert(row.parse(0)?, row.parse(1)?);
    }
    // A condition names an entity as a JSON string, so its quoted key is searched for.
    let selects = [
        (
            "SELECT graph_id FROM answer_entities WHERE entity = ?1",
            false,
        ),
        (
            "SELECT graph_id FROM role_fill_entities WHERE entity = ?1",
            false,
        ),
        (
            "SELECT graph_id FROM participation_entities WHERE entity = ?1",
            false,
        ),
        (
            "SELECT graph_id FROM nodes WHERE relevant_when IS NOT NULL \
             AND instr(relevant_when, ?1) > 0",
            true,
        ),
    ];
    let mut found = BTreeMap::new();
    for key in backend::with_aliases(entities, &aliases) {
        for (select, quoted) in selects {
            let param = if quoted {
                format!("\"{key}\"")
            } else {
                key.to_string()
            };
            for row in rows(connection, select, vec![text(&param)]).await? {
                let Some(journey) = journey_of_graph(&row.text(0)?)? else {
                    continue;
                };
                if found.contains_key(&journey) {
                    continue;
                }
                let of = cairn_schema::RevisionOf::Domain(cairn_schema::Domain::Journey(
                    journey.clone(),
                ));
                found.insert(journey, load::revision_of(connection, &of).await?);
            }
        }
    }
    Ok(found)
}

pub(crate) async fn resolve_entity(
    connection: &Connection,
    key: &EntityKey,
) -> Result<Option<Entity>, StoreError> {
    let alias = first(
        connection,
        "SELECT entity FROM entity_aliases WHERE alias = ?1",
        vec![text(key)],
    )
    .await?;
    let key: EntityKey = match alias {
        Some(row) => row.parse(0)?,
        None => key.clone(),
    };
    let Some(row) = first(
        connection,
        "SELECT name FROM entities WHERE key = ?1",
        vec![text(&key)],
    )
    .await?
    else {
        return Ok(None);
    };
    let mut emails = BTreeSet::new();
    let select = "SELECT email FROM entity_emails WHERE entity = ?1";
    for email in rows(connection, select, vec![text(&key)]).await? {
        emails.insert(email.parse(0)?);
    }
    Ok(Some(Entity {
        key,
        name: row.parse(0)?,
        emails,
    }))
}

/// J5: the WHERE clause and parameters for an event query.
fn event_filter(query: &EventQuery, horizon: i64) -> Result<(String, Vec<Value>), StoreError> {
    let mut conditions = vec!["e.seq < ?1".to_owned()];
    let mut params: Vec<Value> = vec![int(horizon)];
    let mut slot = |condition: &str, value: Value| {
        params.push(value);
        conditions.push(condition.replace('?', &format!("?{}", params.len())));
    };
    if let Some(after) = query.after {
        slot("e.seq > ?", int(i64::try_from(after).unwrap_or(i64::MAX)));
    }
    if let Some(log) = &query.log {
        let (kind, id) = domain_columns(log);
        slot("e.log_kind = ?", text(kind));
        slot("e.log_id IS ?", id);
    }
    if let Some(user) = &query.user {
        slot("e.actor_user = ?", text(user));
    }
    if let Some(patch) = &query.patch {
        slot("e.patch_id = ?", text(patch));
    }
    if let Some(from) = query.from {
        slot("e.at >= ?", time(from)?);
    }
    if let Some(until) = query.until {
        slot("e.at < ?", time(until)?);
    }
    if let Some(node) = &query.node {
        slot(
            "e.seq IN (SELECT seq FROM event_nodes WHERE node = ?)",
            text(node),
        );
    }
    if !query.types.is_empty() {
        let mut names = Vec::new();
        for event_type in &query.types {
            params.push(json_enum(event_type)?);
            names.push(format!("?{}", params.len()));
        }
        conditions.push(format!("e.event_type IN ({})", names.join(", ")));
    }
    Ok((format!("WHERE {}", conditions.join(" AND ")), params))
}

/// J5. Only events below `horizon` are read: every position below it belongs to a
/// transaction that ended before the read began (`crate::sequence`).
pub(crate) async fn events(
    connection: &Connection,
    query: &EventQuery,
    horizon: i64,
) -> Result<Page<LoggedEvent, u64>, StoreError> {
    let (filter, params) = event_filter(query, horizon)?;
    let select = format!(
        "SELECT e.seq, e.patch_id, e.ordinal, e.log_kind, e.log_id, e.event_type, e.actor_user, \
         e.agent, e.confirming_user, e.subject, e.at, e.note, e.delta FROM events e {filter} \
         ORDER BY e.seq LIMIT {}",
        query.size.get() + 1
    );
    let mut items = Vec::new();
    for row in rows(connection, &select, params).await? {
        items.push(logged_event(&row)?);
    }
    Ok(Page::cut(items, query.size, |logged| logged.seq))
}

/// An event row, rebuilt through its written form.
fn logged_event(row: &crate::sql::Row) -> Result<LoggedEvent, StoreError> {
    let event = Event {
        patch_id: row.parse(1)?,
        ordinal: row.number(2)?,
        log: crate::sql::domain_from(&row.text(3)?, row.opt_text(4)?)?,
        event_type: row.name(5)?,
        actor: Actor {
            user: row.parse(6)?,
            agent: row.opt_parse(7)?,
        },
        confirming_user: row.opt_parse(8)?,
        subject: row.json(9)?,
        at: row.timestamp(10)?,
        note: row.opt_parse(11)?,
        delta: row.json(12)?,
    };
    let seq = u64::try_from(row.int(0)?).map_err(|error| corrupt(&format!("seq: {error}")))?;
    Ok(LoggedEvent { seq, event })
}

/// A LIKE pattern matching `needle` anywhere, its wildcards escaped. LIKE compares ASCII
/// letters case-insensitively and every other character as written, as the memory
/// backend's search does.
fn contains_pattern(needle: &str) -> String {
    let mut pattern = String::from("%");
    for character in needle.chars() {
        if matches!(character, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// Every journey where `pattern` matches, and where.
async fn search_hits(
    connection: &Connection,
    pattern: &str,
) -> Result<BTreeMap<JourneyId, BTreeSet<SearchHit>>, StoreError> {
    let like = |column: &str| format!("{column} LIKE ?1 ESCAPE '\\'");
    let in_journeys = "graph_id LIKE 'journey/%'";
    let mut hits: BTreeMap<JourneyId, BTreeSet<SearchHit>> = BTreeMap::new();
    for (column, hit) in [
        ("name", SearchHit::JourneyName),
        ("description", SearchHit::JourneyDescription),
    ] {
        let select = format!("SELECT id FROM journeys WHERE {}", like(column));
        for row in rows(connection, &select, vec![text(pattern)]).await? {
            hits.entry(row.parse(0)?).or_default().insert(hit.clone());
        }
    }
    let selects = [
        format!(
            "SELECT graph_id, key FROM nodes WHERE {in_journeys} AND {}",
            like("title")
        ),
        format!(
            "SELECT graph_id, key FROM nodes WHERE {in_journeys} AND {}",
            like("description")
        ),
        format!(
            "SELECT graph_id, key FROM annotations WHERE {in_journeys} AND ({} OR {})",
            like("title"),
            like("content")
        ),
        format!(
            "SELECT graph_id, key, node FROM resources WHERE {in_journeys} AND ({} OR {})",
            like("title"),
            like("body")
        ),
    ];
    for (index, select) in selects.iter().enumerate() {
        for row in rows(connection, select, vec![text(pattern)]).await? {
            let Some(journey) = journey_of_graph(&row.text(0)?)? else {
                continue;
            };
            let key = row.text(1)?;
            let hit = match index {
                0 => SearchHit::NodeTitle(parse(&key)?),
                1 => SearchHit::NodeDescription(parse(&key)?),
                2 => SearchHit::Annotation(parse(&key)?),
                _ => SearchHit::Resource {
                    node: row.parse(2)?,
                    resource: parse(&key)?,
                },
            };
            hits.entry(journey).or_default().insert(hit);
        }
    }
    Ok(hits)
}

pub(crate) async fn search(
    connection: &Connection,
    query: &SearchQuery,
) -> Result<Page<JourneyMatches, JourneyId>, StoreError> {
    let hits = search_hits(connection, &contains_pattern(query.text.as_str())).await?;
    let mut items = Vec::new();
    for (journey, found) in hits {
        if query.after.as_ref().is_some_and(|after| journey <= *after) {
            continue;
        }
        let Some((header, _)) = load::journey_row(connection, &journey).await? else {
            continue;
        };
        items.push(JourneyMatches {
            journey,
            name: header.name,
            hits: found,
        });
        if items.len() > query.size.len() {
            break;
        }
    }
    Ok(Page::cut(items, query.size, |matches| {
        matches.journey.clone()
    }))
}

#[cfg(test)]
mod tests {
    use super::contains_pattern;

    #[test]
    fn search_patterns_escape_like_wildcards() {
        let cases = [
            ("review", "%review%"),
            ("50%", "%50\\%%"),
            ("a_b", "%a\\_b%"),
            ("back\\slash", "%back\\\\slash%"),
        ];
        for (needle, pattern) in cases {
            assert_eq!(contains_pattern(needle), pattern);
        }
    }
}
