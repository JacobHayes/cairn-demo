//! Conformance cases for the store's queries: alias resolution (E6), current revisions
//! (H6), the journey index (C16), route detail (C17), event history (J5), text search, and
//! paging.

use std::collections::{BTreeMap, BTreeSet};

use cairn_schema::{
    Domain, EventType, GraphId, GraphRecord, Insertion, JourneyId, JourneyStatus, Lineage, Record,
    Subject, ViolationCode, Write,
};
use serde_json::json;

use super::{Backend, applied, failed, open};
use crate::build::{
    self, action, create_journey, create_route, deployment_patch, entity, id, journey_graph,
    journey_header, journey_patch, node, node_key, put_in, revision, route_patch, version,
};
use crate::commit::CommitError;
use crate::query::{
    EventQuery, InsertionUse, JourneyQuery, PageSize, SearchHit, SearchQuery, VersionJourneys,
};
use crate::store::Store;

fn keys(items: &[&str]) -> Vec<JourneyId> {
    items.iter().map(|item| id(item)).collect()
}

/// E6: an alias resolves to the entity it was merged into, and a journey referring to the
/// old key counts as referring to that entity; an alias must point at an entity.
pub async fn aliases_resolve_to_their_entity<B: Backend>(backend: &B) {
    let store = open(backend).await;
    let setup = deployment_patch("p_people", 0).event(
        EventType::EntitiesMerged,
        Subject::Entity(id("e_old")),
        vec![
            Write::Put(Record::Entity(entity("e_a", "A", &["a@example.org"]))),
            Write::Put(Record::EntityAlias {
                alias: id("e_old"),
                entity: id("e_a"),
            }),
            Write::Put(Record::EntityAlias {
                alias: id("e_older"),
                entity: id("e_a"),
            }),
        ],
    );
    applied(&store, setup.commit()).await;
    let answer = create_journey(
        "p_one",
        "j_one",
        vec![node(json!({
            "key": "n_who", "id": "who", "kind": "decision", "title": "Who",
            "prompt": "Who?", "answer_type": "entity",
        }))],
    )
    .event(
        EventType::AnswerSet,
        Subject::Node(node_key("n_who")),
        vec![put_in(
            &journey_graph("j_one"),
            GraphRecord::Answer {
                decision: node_key("n_who"),
                value: cairn_schema::AnswerValue::Entity(id("e_old")),
                rationale: None,
            },
        )],
    );
    applied(&store, answer.commit()).await;
    let fill = create_journey("p_two", "j_two", Vec::new()).event(
        EventType::RoleFillChanged,
        Subject::Role(id("r_owner")),
        vec![put_in(
            &journey_graph("j_two"),
            GraphRecord::RoleFill {
                role: id("r_owner"),
                entities: cairn_schema::BoundedSet::new([id::<cairn_schema::EntityKey>("e_older")])
                    .unwrap(),
            },
        )],
    );
    applied(&store, fill.commit()).await;
    applied(
        &store,
        create_journey("p_three", "j_three", Vec::new()).commit(),
    )
    .await;

    let a = Some(entity("e_a", "A", &["a@example.org"]));
    assert_eq!(store.resolve_entity(&id("e_old")).await.unwrap(), a);
    assert_eq!(store.resolve_entity(&id("e_a")).await.unwrap(), a);
    assert_eq!(store.resolve_entity(&id("e_nobody")).await.unwrap(), None);
    // Any key of an entity finds every journey holding any key of it.
    for asked in ["e_a", "e_old", "e_older"] {
        let referencing = store
            .journeys_referencing(&BTreeSet::from([id(asked)]))
            .await
            .unwrap();
        assert_eq!(
            referencing,
            BTreeMap::from([(id("j_one"), revision(1)), (id("j_two"), revision(1))]),
            "{asked}"
        );
    }

    let dangling = deployment_patch("p_dangling", 1).event(
        EventType::EntitiesMerged,
        Subject::Entity(id("e_gone")),
        vec![Write::Put(Record::EntityAlias {
            alias: id("e_gone"),
            entity: id("e_missing"),
        })],
    );
    let error = failed(&store, dangling.commit()).await;
    let CommitError::Rejected(cairn_schema::Rejection::Invalid { violations }) = error else {
        panic!("expected an invalid rejection, got {error:?}");
    };
    let codes: Vec<_> = violations.as_slice().iter().map(|v| v.code).collect();
    assert_eq!(codes, vec![ViolationCode::EntityUnresolved]);
}

/// H6: the current revisions list every journey, route, proposal, and the deployment.
pub async fn current_revisions_list_every_domain_and_proposal<B: Backend>(backend: &B) {
    let store = open(backend).await;
    assert_eq!(
        store.revisions().await.unwrap(),
        crate::query::Revisions::default()
    );
    applied(
        &store,
        create_journey("p_one", "j_one", Vec::new()).commit(),
    )
    .await;
    applied(
        &store,
        journey_patch("p_two", "j_one", 1)
            .event(
                EventType::NodeAdded,
                Subject::Node(node_key("n_a")),
                vec![put_in(
                    &journey_graph("j_one"),
                    GraphRecord::Node(action("n_a", "a", None)),
                )],
            )
            .commit(),
    )
    .await;
    applied(&store, create_route("p_route", "vendor").commit()).await;
    let destination = Domain::Route(id("vendor"));
    let proposal = build::proposal_patch("p_proposal", "pr_one", destination.clone(), 0).event(
        EventType::ProposalCreated,
        Subject::Proposal(id("pr_one")),
        vec![Write::Put(Record::Proposal(build::proposal(
            "pr_one",
            &destination,
            1,
        )))],
    );
    applied(&store, proposal.commit()).await;
    let revisions = store.revisions().await.unwrap();
    assert_eq!(revisions.deployment, revision(0));
    assert_eq!(
        revisions.journeys,
        BTreeMap::from([(id("j_one"), revision(2))])
    );
    assert_eq!(
        revisions.routes,
        BTreeMap::from([(id("vendor"), revision(1))])
    );
    assert_eq!(
        revisions.proposals,
        BTreeMap::from([(id("pr_one"), (destination, revision(1)))])
    );
}

/// Routes `vendor` (versions 1 and 2) and `hiring` (version 1), and four journeys:
/// `j_a` on vendor 1 (active), `j_b` on vendor 2 (completed), `j_c` on hiring 1 (archived),
/// and `j_d` with no route (active), which refers to `e_x`.
async fn index_fixture<S: Store>(store: &S) {
    applied(store, create_route("p_vendor", "vendor").commit()).await;
    applied(
        store,
        build::publish(
            // The one root an insertion of this version is copied from (C19).
            route_patch("p_v1", "vendor", 1).event(
                EventType::NodeAdded,
                Subject::Node(node_key("n_root")),
                vec![put_in(
                    &GraphId::RouteDraft(id("vendor")),
                    GraphRecord::Node(action("n_root", "root", None)),
                )],
            ),
            "vendor",
            1,
            1,
        )
        .commit(),
    )
    .await;
    applied(
        store,
        build::publish(
            route_patch("p_v2", "vendor", 2).event(
                EventType::NodeAdded,
                Subject::Node(node_key("n_second")),
                vec![put_in(
                    &GraphId::RouteDraft(id("vendor")),
                    GraphRecord::Node(action("n_second", "second", None)),
                )],
            ),
            "vendor",
            2,
            2,
        )
        .commit(),
    )
    .await;
    applied(store, create_route("p_hiring", "hiring").commit()).await;
    applied(
        store,
        build::publish(route_patch("p_h1", "hiring", 1), "hiring", 1, 3).commit(),
    )
    .await;
    let journeys = [
        ("j_a", Some(("vendor", 1)), JourneyStatus::Active),
        ("j_b", Some(("vendor", 2)), JourneyStatus::Completed),
        ("j_c", Some(("hiring", 1)), JourneyStatus::Archived),
        ("j_d", None, JourneyStatus::Active),
    ];
    for (journey, lineage, status) in journeys {
        let mut header = journey_header(journey, journey, lineage);
        header.status = status;
        let mut builder = journey_patch(&format!("p_{journey}"), journey, 0).event(
            EventType::JourneyCreated,
            Subject::Journey(id(journey)),
            vec![Write::Put(Record::JourneyHeader(header))],
        );
        if journey == "j_d" {
            builder = builder.event(
                EventType::NodeAdded,
                Subject::Node(node_key("n_task")),
                vec![put_in(
                    &journey_graph(journey),
                    GraphRecord::Node(node(json!({
                        "key": "n_task", "id": "task", "kind": "action", "title": "Task",
                        "participations": {"k_owner": ["e_x"]},
                    }))),
                )],
            );
            // C19: an insertion of vendor's first version, whose second is later.
            builder = builder.event(
                EventType::SegmentInserted,
                Subject::Insertion(id("i_task")),
                vec![put_in(
                    &journey_graph(journey),
                    GraphRecord::Insertion(Insertion {
                        key: id("i_task"),
                        segment: Lineage {
                            route: id("vendor"),
                            version: version(1),
                        },
                        parent: None,
                        roles: BTreeMap::new(),
                        kinds: BTreeMap::new(),
                        nodes: BTreeMap::from([(node_key("n_task"), id("n_root"))]),
                    }),
                )],
            );
        }
        applied(store, builder.commit()).await;
    }
}

/// C16: the journey index filters by status, route, version, the entities referred to
/// ("mine", narrowed), and whether an upgrade is available; every filter given must hold.
pub async fn the_journey_index_filters_by_status_route_version_reference_and_upgrade<B: Backend>(
    backend: &B,
) {
    let store = open(backend).await;
    index_fixture(&store).await;
    let query = JourneyQuery::default;
    let cases = [
        (query(), vec!["j_a", "j_b", "j_c", "j_d"]),
        (
            JourneyQuery {
                statuses: BTreeSet::from([JourneyStatus::Active]),
                ..query()
            },
            vec!["j_a", "j_d"],
        ),
        (
            JourneyQuery {
                statuses: BTreeSet::from([JourneyStatus::Completed, JourneyStatus::Archived]),
                ..query()
            },
            vec!["j_b", "j_c"],
        ),
        (
            JourneyQuery {
                route: Some(id("vendor")),
                ..query()
            },
            vec!["j_a", "j_b"],
        ),
        (
            JourneyQuery {
                version: Some(version(1)),
                ..query()
            },
            vec!["j_a", "j_c"],
        ),
        (
            JourneyQuery {
                referencing: Some(BTreeSet::from([id("e_x")])),
                ..query()
            },
            vec!["j_d"],
        ),
        (
            JourneyQuery {
                upgrade_available: Some(true),
                ..query()
            },
            vec!["j_a"],
        ),
        (
            JourneyQuery {
                upgrade_available: Some(false),
                ..query()
            },
            vec!["j_b", "j_c", "j_d"],
        ),
        (
            JourneyQuery {
                route: Some(id("vendor")),
                upgrade_available: Some(false),
                ..query()
            },
            vec!["j_b"],
        ),
    ];
    for (query, expected) in cases {
        let page = store.journeys(&query).await.unwrap();
        let found: Vec<_> = page
            .items
            .iter()
            .map(|summary| summary.id.clone())
            .collect();
        assert_eq!(found, keys(&expected), "{query:?}");
        assert_eq!(page.next, None);
    }
    let page = store.journeys(&query()).await.unwrap();
    let counts: Vec<_> = page
        .items
        .iter()
        .map(|summary| summary.latest_version_node_count)
        .collect();
    assert_eq!(counts, vec![Some(2), Some(2), Some(0), None]);
    let a = &page.items[0];
    assert_eq!(
        (a.revision, a.latest_version, a.upgrade_available()),
        (revision(1), Some(version(2)), true)
    );
}

/// The journey index, event history, and search page by a cursor: each page holds at most
/// its size, and the pages together hold every item once, in order.
pub async fn the_journey_index_events_and_search_page<B: Backend>(backend: &B) {
    let store = open(backend).await;
    let names = ["j_1", "j_2", "j_3", "j_4", "j_5"];
    for journey in names {
        let create = create_journey(
            &format!("p_{journey}"),
            journey,
            vec![action("n_common", "common", None)],
        );
        applied(&store, create.commit()).await;
    }
    let size = PageSize::new(2);

    let mut journeys = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let query = JourneyQuery {
            after: after.clone(),
            size,
            ..JourneyQuery::default()
        };
        let page = store.journeys(&query).await.unwrap();
        assert!(page.items.len() <= 2);
        journeys.extend(page.items.into_iter().map(|summary| summary.id));
        pages += 1;
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!((journeys, pages), (keys(&names), 3));

    let mut events = Vec::new();
    let mut after = None;
    loop {
        let query = EventQuery {
            after,
            size,
            ..EventQuery::default()
        };
        let page = store.events(&query).await.unwrap();
        events.extend(page.items.into_iter().map(|logged| logged.event.patch_id));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let expected: Vec<_> = names
        .iter()
        .flat_map(|journey| [id(&format!("p_{journey}")), id(&format!("p_{journey}"))])
        .collect();
    assert_eq!(events, expected);

    let mut found = Vec::new();
    let mut after = None;
    loop {
        let query = SearchQuery {
            text: id("common"),
            after: after.clone(),
            size,
        };
        let page = store.search(&query).await.unwrap();
        found.extend(page.items.into_iter().map(|matches| matches.journey));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(found, keys(&names));
}

/// C17, C19: route detail lists a route's versions, oldest first, with the journeys on each
/// and the insertions of it.
pub async fn route_detail_lists_versions_with_their_journeys<B: Backend>(backend: &B) {
    let store = open(backend).await;
    index_fixture(&store).await;
    let detail = store.route_detail(&id("vendor")).await.unwrap().unwrap();
    assert_eq!(detail.header, build::route_header("vendor", "vendor"));
    assert_eq!(detail.revision, revision(3));
    assert_eq!(
        detail.versions,
        vec![
            VersionJourneys {
                version: version(1),
                published_at: build::at(1),
                journeys: BTreeSet::from([id("j_a")]),
                insertions: vec![InsertionUse {
                    host: Domain::Journey(id("j_d")),
                    graph: journey_graph("j_d"),
                    insertion: id("i_task"),
                    title: build::title("Task"),
                    upgrade_available: true,
                }],
            },
            VersionJourneys {
                version: version(2),
                published_at: build::at(2),
                journeys: BTreeSet::from([id("j_b")]),
                insertions: Vec::new(),
            },
        ]
    );
    assert_eq!(store.route_detail(&id("unknown")).await.unwrap(), None);
}

/// J5: events filter by log, node, user, type, patch, and time range, and every filter
/// given must hold.
pub async fn events_filter_by_journey_node_user_type_patch_and_time<B: Backend>(backend: &B) {
    let store = open(backend).await;
    let one = create_journey("p_one", "j_one", vec![action("n_a", "a", None)]).by("u_ann");
    applied(&store, one.at(0).commit()).await;
    let two = journey_patch("p_two", "j_one", 1).at(10).by("u_bob").event(
        EventType::AnnotationAdded,
        Subject::Attachment(id("a_note")),
        vec![put_in(
            &journey_graph("j_one"),
            GraphRecord::Annotation(build::note("a_note", Some("n_a"), "On a.")),
        )],
    );
    applied(&store, two.commit()).await;
    let other = create_journey("p_other", "j_other", vec![action("n_a", "a", None)])
        .at(20)
        .by("u_bob");
    applied(&store, other.commit()).await;

    let journey = |key: &str| Some(Domain::Journey(id(key)));
    let query = EventQuery::default;
    let cases = [
        (
            EventQuery {
                log: journey("j_one"),
                ..query()
            },
            vec![("p_one", 0), ("p_one", 1), ("p_two", 0)],
        ),
        (
            EventQuery {
                node: Some(node_key("n_a")),
                ..query()
            },
            vec![("p_one", 1), ("p_two", 0), ("p_other", 1)],
        ),
        (
            EventQuery {
                log: journey("j_one"),
                node: Some(node_key("n_a")),
                ..query()
            },
            vec![("p_one", 1), ("p_two", 0)],
        ),
        (
            EventQuery {
                user: Some(id("u_bob")),
                ..query()
            },
            vec![("p_two", 0), ("p_other", 0), ("p_other", 1)],
        ),
        (
            EventQuery {
                types: BTreeSet::from([EventType::JourneyCreated, EventType::AnnotationAdded]),
                ..query()
            },
            vec![("p_one", 0), ("p_two", 0), ("p_other", 0)],
        ),
        (
            EventQuery {
                patch: Some(id("p_other")),
                ..query()
            },
            vec![("p_other", 0), ("p_other", 1)],
        ),
        (
            EventQuery {
                from: Some(build::at(10)),
                until: Some(build::at(20)),
                ..query()
            },
            vec![("p_two", 0)],
        ),
        (
            EventQuery {
                user: Some(id("u_ann")),
                patch: Some(id("p_two")),
                ..query()
            },
            vec![],
        ),
    ];
    for (query, expected) in cases {
        let page = store.events(&query).await.unwrap();
        let found: Vec<_> = page
            .items
            .iter()
            .map(|logged| {
                (
                    logged.event.patch_id.as_str().to_owned(),
                    logged.event.ordinal,
                )
            })
            .collect();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(patch, ordinal)| (patch.to_owned(), ordinal))
            .collect();
        assert_eq!(found, expected, "{query:?}");
    }
    let page = store.events(&query()).await.unwrap();
    let seqs: Vec<_> = page.items.iter().map(|logged| logged.seq).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");

    // A removed node takes the edges into it: the nodes that required it have the removal
    // in their history.
    let graph = journey_graph("j_three");
    let edge = cairn_schema::Edge {
        node: node_key("n_y"),
        requires: node_key("n_x"),
    };
    let create = create_journey(
        "p_three",
        "j_three",
        vec![action("n_x", "x", None), action("n_y", "y", None)],
    )
    .event(
        EventType::EdgeChanged,
        Subject::Edge(edge.clone()),
        vec![put_in(&graph, GraphRecord::Edge(edge))],
    );
    applied(&store, create.commit()).await;
    let removal = journey_patch("p_remove", "j_three", 1).event(
        EventType::NodeRemoved,
        Subject::Node(node_key("n_x")),
        vec![crate::build::remove_in(
            &graph,
            cairn_schema::GraphKey::Node(node_key("n_x")),
        )],
    );
    applied(&store, removal.commit()).await;
    let history = EventQuery {
        log: journey("j_three"),
        node: Some(node_key("n_y")),
        ..query()
    };
    let patches: Vec<_> = store
        .events(&history)
        .await
        .unwrap()
        .items
        .iter()
        .map(|logged| logged.event.patch_id.as_str().to_owned())
        .collect();
    assert_eq!(patches, vec!["p_three", "p_three", "p_remove"]);
}

/// Text search across journeys hits journey names and descriptions, node titles and
/// descriptions, notes and links, and resources, comparing ASCII letters case-insensitively
/// and every other character as written.
pub async fn text_search_hits_names_nodes_notes_and_resources<B: Backend>(backend: &B) {
    let store = open(backend).await;
    let graph = journey_graph("j_one");
    let mut header = journey_header("j_one", "Quarterly Review", None);
    header.description = Some(id("Covers the 50% milestone."));
    let task = node(json!({
        "key": "n_task", "id": "task", "kind": "action", "title": "Draft the review",
        "description": "Ünicode Notes",
        "resources": [
            {"key": "a_tip", "title": "Hint", "tip": "Reuse last quarter's REVIEW."},
            {"key": "a_link", "reference": "https://example.org/review-guide"},
        ],
    }));
    let create = journey_patch("p_one", "j_one", 0).event(
        EventType::JourneyCreated,
        Subject::Journey(id("j_one")),
        vec![
            Write::Put(Record::JourneyHeader(header)),
            put_in(&graph, GraphRecord::Node(task)),
            put_in(
                &graph,
                GraphRecord::Annotation(build::note("a_note", None, "Reviewed twice.")),
            ),
        ],
    );
    applied(&store, create.commit()).await;
    applied(
        &store,
        create_journey("p_two", "j_two", vec![action("n_x", "x", None)]).commit(),
    )
    .await;

    let resource = |key: &str| SearchHit::Resource {
        node: node_key("n_task"),
        resource: id(key),
    };
    let cases = [
        (
            "review",
            vec![
                SearchHit::JourneyName,
                SearchHit::NodeTitle(node_key("n_task")),
                SearchHit::Annotation(id("a_note")),
                resource("a_tip"),
                resource("a_link"),
            ],
        ),
        ("50%", vec![SearchHit::JourneyDescription]),
        ("unicode", vec![]),
        (
            "Ünicode notes",
            vec![SearchHit::NodeDescription(node_key("n_task"))],
        ),
        ("hint", vec![resource("a_tip")]),
        ("nowhere", vec![]),
    ];
    for (text, expected) in cases {
        let query = SearchQuery {
            text: id(text),
            after: None,
            size: PageSize::MAX,
        };
        let page = store.search(&query).await.unwrap();
        let expected: BTreeSet<_> = expected.into_iter().collect();
        if expected.is_empty() {
            assert!(page.items.is_empty(), "{text}: {page:?}");
            continue;
        }
        assert_eq!(page.items.len(), 1, "{text}");
        assert_eq!(page.items[0].journey, id("j_one"));
        assert_eq!(page.items[0].hits, expected, "{text}");
    }
}
