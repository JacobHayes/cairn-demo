//! A commit as one MVCC transaction (ARCHITECTURE, Concurrency and notification): `BEGIN
//! CONCURRENT`, whose first write is the domain's revision row, so commits to one domain
//! conflict and commits to different domains do not wait on each other. In process,
//! commits on the same rows never meet in flight: each first takes its turn at what it
//! writes (`queue`). A write-write conflict Turso still reports (a connection outside those
//! turns) is retried once from the start, then answered by what has moved.

use std::collections::BTreeSet;

use cairn_schema::{
    Actor, Domain, Event, GraphId, JourneyHeader, PatchReceipt, PatchTarget, Record, Revision,
    RevisionConflict, RevisionOf, RouteHeader, TouchedSet, Write,
};
use cairn_store::backend::{self, Shape, StoredReceipt};
use cairn_store::{Commit, CommitError, CommitPoint, Committed, Faults, Precondition, StoreError};
use turso::Connection;

use crate::durable::Durability;
use crate::load;
use crate::queue::Claim;
use crate::sequence::Sequencer;
use crate::sql::{
    Abort, SqlError, domain_columns, execute, first, int, json, json_enum, opt_int, opt_text, rows,
    text, time,
};
use crate::write::Writer;

/// The fields a domain's first commit writes: its revision row is written with them.
enum Created {
    Journey(JourneyHeader),
    Route(RouteHeader),
}

/// A17: a journey's or route's first commit (base revision 0) writes its fields, and its
/// revision row is inserted with the last of them.
fn created(commit: &Commit) -> Result<Option<Created>, StoreError> {
    if commit.base_revision != Revision::NONE {
        return Ok(None);
    }
    let mut created = None;
    for write in commit.change_set.writes() {
        match (write, &commit.target) {
            (Write::Put(Record::JourneyHeader(header)), PatchTarget::Journey(id))
                if header.id == *id =>
            {
                created = Some(Created::Journey(header.clone()));
            }
            (Write::Put(Record::RouteHeader(header)), PatchTarget::Route(id))
                if header.id == *id =>
            {
                created = Some(Created::Route(header.clone()));
            }
            _ => {}
        }
    }
    let creates_domain = matches!(
        commit.target,
        PatchTarget::Journey(_) | PatchTarget::Route(_)
    );
    if creates_domain && created.is_none() {
        return Err(StoreError::Malformed(format!(
            "{} has no fields after its commit: its first commit writes them",
            commit.target.domain()
        )));
    }
    Ok(created)
}

/// H5: what a commit takes its turn at before it begins: every revision row it writes
/// (its target's, and its dependencies' and the deployment's when it writes them) and its
/// patch id, so a commit beside one in flight on any of them waits for it to end.
pub(crate) fn claims(commit: &Commit, shape: &Shape) -> BTreeSet<Claim> {
    let mut rows = vec![
        shape.advances.clone(),
        RevisionOf::Domain(shape.domain.clone()),
    ];
    rows.extend(dependencies(commit, shape));
    if shape.bumps_deployment || shape.writes_deployment {
        rows.push(RevisionOf::Domain(Domain::Deployment));
    }
    let patch = Claim::Patch(commit.change_set.receipt.patch_id.clone());
    rows.into_iter()
        .map(Claim::Revision)
        .chain(std::iter::once(patch))
        .collect()
}

/// Commits on `connection`, retrying once on a write-write conflict. A commit that fails
/// other than by a conflict or a constraint fails the store closed.
pub(crate) async fn commit(
    connection: &Connection,
    durability: &Durability,
    faults: &Faults,
    log: &Sequencer,
    commit: &Commit,
    shape: &Shape,
) -> Result<Committed, CommitError> {
    let created = created(commit)?;
    for _ in 0..2 {
        // The log positions are held until the attempt's transaction has ended.
        let positions = log.reserve(commit.change_set.events.len());
        let attempted = attempt(
            connection,
            durability,
            faults,
            commit,
            shape,
            created.as_ref(),
            positions.first(),
        );
        match attempted.await {
            Ok(committed) => return Ok(committed),
            Err(Abort::Answer(error)) => return Err(error),
            Err(Abort::Conflict) => {}
        }
    }
    // H5: a resubmission is never answered as stale, even one that lost both attempts to
    // a commit in flight before it reached its receipt.
    let receipt = &commit.change_set.receipt;
    if let Some(stored) = load::receipt(connection, &receipt.patch_id).await? {
        return backend::resubmission(&stored.receipt, receipt);
    }
    Err(conflicted(connection, commit, shape).await)
}

async fn rollback(connection: &Connection) {
    // After a write-write conflict Turso has already rolled the transaction back, and a
    // failed rollback leaves nothing to undo either way.
    let _ = execute(connection, "ROLLBACK", Vec::new()).await;
}

#[tracing::instrument(level = "info", skip_all,
    fields(sentry.op = "db.transaction", sentry.name = "commit", db.system = "sqlite"))]
async fn attempt(
    connection: &Connection,
    durability: &Durability,
    faults: &Faults,
    commit: &Commit,
    shape: &Shape,
    created: Option<&Created>,
    first_position: i64,
) -> Result<Committed, Abort> {
    execute(connection, "BEGIN CONCURRENT", Vec::new()).await?;
    let outcome = within(connection, faults, commit, shape, created, first_position).await;
    match outcome {
        Ok(Committed::Applied(receipt)) => match execute(connection, "COMMIT", Vec::new()).await {
            Ok(_) => Ok(Committed::Applied(receipt)),
            Err(error) => {
                // A conflict or a constraint fails the commit before its log record is
                // written; anything else may fail it after (the log's sync), so the store
                // cannot say whether the commit holds (`durable`). Closed before anything
                // awaits, so no call is answered in between.
                if let SqlError::Other(reason) = &error {
                    durability.fail_closed(reason);
                }
                rollback(connection).await;
                match error {
                    SqlError::Conflict(_) => Err(Abort::Conflict),
                    SqlError::Constraint(reason) => {
                        Err(Abort::Answer(CommitError::Failed(StoreError::Malformed(
                            format!("the commit leaves a row without its parent: {reason}"),
                        ))))
                    }
                    SqlError::Other(reason) => Err(StoreError::Backend(reason).into()),
                }
            }
        },
        Ok(answered) => {
            rollback(connection).await;
            Ok(answered)
        }
        Err(abort) => {
            rollback(connection).await;
            Err(abort)
        }
    }
}

async fn pause(faults: &Faults, point: CommitPoint) {
    if let Some(wait) = faults.pause_at(point) {
        wait.await;
    }
}

/// Everything inside the transaction, in the order ARCHITECTURE's Store trait gives.
async fn within(
    connection: &Connection,
    faults: &Faults,
    commit: &Commit,
    shape: &Shape,
    created: Option<&Created>,
    first_position: i64,
) -> Result<Committed, Abort> {
    let current = lock(connection, commit, shape, created).await?;
    lock_dependencies(connection, commit, shape).await?;
    pause(faults, CommitPoint::AfterRevisionRow).await;
    let receipt = &commit.change_set.receipt;
    if let Some(stored) = load::receipt(connection, &receipt.patch_id).await? {
        return Ok(backend::resubmission(&stored.receipt, receipt)?);
    }
    check_revisions(connection, commit, shape, current).await?;
    let mut violations = Vec::new();
    if let (Domain::Journey(journey), true) =
        (&shape.domain, commit.base_revision == Revision::NONE)
    {
        let select = "SELECT 1 FROM deleted_journeys WHERE id = ?1";
        if first(connection, select, vec![text(journey)])
            .await?
            .is_some()
        {
            violations.push(backend::deleted_journey(journey));
        }
    }
    if !shape.created_entities.is_empty() {
        let before = load::deployment(connection).await?;
        violations.extend(backend::entity_create_violations(shape, &before)?);
    }
    let nodes = event_nodes(connection, commit).await?;
    let taken_proposals = proposals_of_deleted(connection, shape).await?;
    let mut writer = Writer::new(connection, shape.revision);
    for write in commit.change_set.writes() {
        writer.apply(write).await?;
    }
    violations.extend(produced_violations(connection, shape).await?);
    backend::reject_violations(violations)?;
    let deployment = advance(connection, shape).await?;
    if faults.take_failure(CommitPoint::BetweenStateAndEvents) {
        return Err(StoreError::Backend(format!(
            "injected fault at {:?}",
            CommitPoint::BetweenStateAndEvents
        ))
        .into());
    }
    pause(faults, CommitPoint::BetweenStateAndEvents).await;
    append_events(connection, commit, first_position, nodes).await?;
    let mut stored = StoredReceipt::new(&commit.change_set, shape, deployment);
    stored
        .proposals
        .extend(taken_proposals.into_iter().map(|id| (id, Revision::NONE)));
    store_receipt(connection, &stored).await?;
    pause(faults, CommitPoint::BeforeCommit).await;
    Ok(Committed::Applied(receipt.clone()))
}

/// J5: the nodes each event is about or wrote on, with the nodes whose edges a whole-node
/// removal in it takes, read before the commit's writes.
async fn event_nodes(
    connection: &Connection,
    commit: &Commit,
) -> Result<Vec<std::collections::BTreeSet<cairn_schema::NodeKey>>, Abort> {
    let mut all = Vec::new();
    for event in &commit.change_set.events {
        let mut nodes = backend::event_nodes(event);
        for (graph, removed) in backend::removed_nodes(event) {
            let select = "SELECT node FROM edges WHERE graph_id = ?1 AND requires = ?2";
            let params = vec![text(&crate::sql::graph_id(&graph)), text(&removed)];
            for row in crate::sql::rows(connection, select, params).await? {
                nodes.insert(row.parse(0)?);
            }
        }
        all.push(nodes);
    }
    Ok(all)
}

/// A19: the proposals a commit's hard delete takes with its journey.
async fn proposals_of_deleted(
    connection: &Connection,
    shape: &Shape,
) -> Result<Vec<cairn_schema::ProposalId>, Abort> {
    let Some(journey) = &shape.deletes_journey else {
        return Ok(Vec::new());
    };
    let select =
        "SELECT id FROM proposals WHERE destination_kind = 'journey' AND destination_id = ?1";
    let mut ids = Vec::new();
    for row in crate::sql::rows(connection, select, vec![text(journey)]).await? {
        ids.push(row.parse(0)?);
    }
    Ok(ids)
}

/// The commit's first write: its domain's revision row, set to the revision it produces (a
/// first commit inserts it with the domain's fields), or for a proposal, the proposal's row.
/// Returns the revision the target held.
async fn lock(
    connection: &Connection,
    commit: &Commit,
    shape: &Shape,
    created: Option<&Created>,
) -> Result<Revision, Abort> {
    let current = load::revision_of(connection, &shape.advances).await?;
    let next = int(shape.revision.get());
    match (&commit.target, created) {
        (_, Some(_)) if current != Revision::NONE => {}
        (_, Some(Created::Journey(header))) => {
            let mut writer = Writer::new(connection, shape.revision);
            writer
                .apply(&Write::Put(Record::JourneyHeader(header.clone())))
                .await?;
        }
        (_, Some(Created::Route(header))) => {
            let mut writer = Writer::new(connection, shape.revision);
            writer
                .apply(&Write::Put(Record::RouteHeader(header.clone())))
                .await?;
        }
        (PatchTarget::Journey(id), None) => {
            let sql = "UPDATE journeys SET revision = ?2 WHERE id = ?1";
            execute(connection, sql, vec![text(id), next]).await?;
        }
        (PatchTarget::Route(id), None) => {
            let sql = "UPDATE routes SET revision = ?2 WHERE id = ?1";
            execute(connection, sql, vec![text(id), next]).await?;
        }
        (PatchTarget::Deployment, None) => {
            let sql = "UPDATE deployment SET revision = ?1 WHERE id = 1";
            execute(connection, sql, vec![next]).await?;
        }
        (PatchTarget::Proposal { id, .. }, None) => {
            let sql = "UPDATE proposals SET revision = revision WHERE id = ?1";
            execute(connection, sql, vec![text(id)]).await?;
        }
    }
    Ok(current)
}

/// Writes (without changing) the revision row of every other domain or proposal the
/// commit depends on, so a concurrent commit there conflicts with this one: Turso checks no
/// read against a concurrent write, only writes
/// (decisions/2026-10-06-a-turso-commit-writes-the-revision-row-of-every-domain.md). Those
/// are every revision a precondition names, a proposal's destination (so a proposal cannot
/// be created while its journey is hard-deleted), and the deployment when the commit writes
/// an entity reference (so a merge cannot miss a journey that starts referencing its
/// entities, E6).
async fn lock_dependencies(
    connection: &Connection,
    commit: &Commit,
    shape: &Shape,
) -> Result<(), Abort> {
    for of in dependencies(commit, shape) {
        let (sql, params) = match &of {
            RevisionOf::Domain(Domain::Journey(id)) => (
                "UPDATE journeys SET revision = revision WHERE id = ?1",
                vec![text(id)],
            ),
            RevisionOf::Domain(Domain::Route(id)) => (
                "UPDATE routes SET revision = revision WHERE id = ?1",
                vec![text(id)],
            ),
            RevisionOf::Domain(Domain::Deployment) => (
                "UPDATE deployment SET revision = revision WHERE id = 1",
                Vec::new(),
            ),
            RevisionOf::Proposal(id) => (
                "UPDATE proposals SET revision = revision WHERE id = ?1",
                vec![text(id)],
            ),
        };
        execute(connection, sql, params).await?;
    }
    Ok(())
}

/// The revision rows besides its target's that a commit writes without changing them.
fn dependencies(commit: &Commit, shape: &Shape) -> Vec<RevisionOf> {
    let mut dependencies: Vec<RevisionOf> = backend::expected_revisions(commit, shape)
        .into_iter()
        .skip(1)
        .map(|(of, _)| of)
        .collect();
    if let PatchTarget::Proposal { destination, .. } = &commit.target {
        dependencies.push(RevisionOf::Domain(destination.clone()));
    }
    if shape.references_entities && shape.domain != Domain::Deployment {
        dependencies.push(RevisionOf::Domain(Domain::Deployment));
    }
    dependencies
}

/// H5: every revision the commit names that has moved.
async fn check_revisions(
    connection: &Connection,
    commit: &Commit,
    shape: &Shape,
    current: Revision,
) -> Result<(), Abort> {
    let mut conflicts = Vec::new();
    for (index, (of, expected)) in backend::expected_revisions(commit, shape)
        .into_iter()
        .enumerate()
    {
        let now = if index == 0 {
            current
        } else {
            load::revision_of(connection, &of).await?
        };
        if now != expected {
            conflicts.push(RevisionConflict {
                of,
                expected,
                current: now,
            });
        }
    }
    for precondition in &commit.preconditions {
        if let Precondition::ReferencingJourneys { entities, journeys } = precondition {
            for (journey, now) in crate::queries::referencing(connection, entities).await? {
                if !journeys.contains(&journey) {
                    conflicts.push(RevisionConflict {
                        of: RevisionOf::Domain(Domain::Journey(journey)),
                        expected: Revision::NONE,
                        current: now,
                    });
                }
            }
        }
    }
    if conflicts.is_empty() {
        return Ok(());
    }
    let intervening = intervening(connection, &conflicts).await?;
    Err(backend::stale(conflicts, intervening).into())
}

/// H5: the touched set of the events of every commit that moved a conflicting revision.
pub(crate) async fn intervening(
    connection: &Connection,
    conflicts: &[RevisionConflict],
) -> Result<TouchedSet, StoreError> {
    let select = format!("SELECT {} FROM patch_receipts", load::RECEIPT_COLUMNS);
    let mut touched = TouchedSet::default();
    for row in rows(connection, &select, Vec::new()).await? {
        let stored = load::stored_receipt(&row)?;
        let moved = conflicts
            .iter()
            .any(|conflict| stored.moved_past(&conflict.of, conflict.expected));
        if !moved {
            continue;
        }
        touched.extend(stored.deployment_touched.clone());
        let select = "SELECT delta FROM events WHERE patch_id = ?1 ORDER BY ordinal";
        for event in rows(connection, select, vec![text(&stored.receipt.patch_id)]).await? {
            let delta: Vec<Write> = event.json(0)?;
            for write in &delta {
                for key in write.keys() {
                    touched.insert(key);
                }
            }
        }
    }
    Ok(touched)
}

/// The checks only the commit can make, on the records it produced.
async fn produced_violations(
    connection: &Connection,
    shape: &Shape,
) -> Result<Vec<cairn_schema::Violation>, Abort> {
    let mut violations = Vec::new();
    for graph in &shape.graphs {
        let content = crate::graph::load(connection, graph).await?;
        if content != cairn_schema::Graph::default() && !owned(connection, graph).await? {
            return Err(StoreError::Malformed(format!(
                "{graph:?} has content but no journey, draft, or version holds it"
            ))
            .into());
        }
        violations.extend(backend::graph_violations(graph, &content));
    }
    if shape.writes_deployment {
        let deployment = load::deployment(connection).await?;
        violations.extend(backend::deployment_violations(&deployment));
        violations.extend(backend::deployment_size_violation(&deployment));
    }
    Ok(violations)
}

async fn owned(connection: &Connection, graph: &GraphId) -> Result<bool, Abort> {
    let (select, params) = match graph {
        GraphId::Journey(journey) => ("SELECT 1 FROM journeys WHERE id = ?1", vec![text(journey)]),
        GraphId::RouteDraft(route) => (
            "SELECT 1 FROM route_drafts WHERE route_id = ?1",
            vec![text(route)],
        ),
        GraphId::RouteVersion { route, version } => (
            "SELECT 1 FROM route_versions WHERE route_id = ?1 AND version_number = ?2",
            vec![text(route), int(version.get())],
        ),
    };
    Ok(first(connection, select, params).await?.is_some())
}

/// The revisions besides the lock: a proposal's must be the one the commit produces, a
/// domain must still have its fields unless it was deleted, and entity creates riding in
/// move the deployment's (E6). Returns the deployment revision after the commit.
async fn advance(connection: &Connection, shape: &Shape) -> Result<Revision, Abort> {
    let after = load::revision_of(connection, &shape.advances).await?;
    let deleted = shape.deletes_journey.is_some();
    if after != shape.revision && !deleted {
        let reason = match &shape.advances {
            RevisionOf::Proposal(id) => format!(
                "a patch to proposal {id} leaves it at {after}, not {}",
                shape.revision
            ),
            RevisionOf::Domain(domain) => {
                format!("{domain} has no fields after its commit: its first commit writes them")
            }
        };
        return Err(StoreError::Malformed(reason).into());
    }
    if shape.bumps_deployment {
        execute(
            connection,
            "UPDATE deployment SET revision = revision + 1 WHERE id = 1",
            Vec::new(),
        )
        .await?;
    }
    Ok(load::revision_of(connection, &RevisionOf::Domain(Domain::Deployment)).await?)
}

/// J1, J2: the events, in order, with the nodes each is about.
async fn append_events(
    connection: &Connection,
    commit: &Commit,
    first_position: i64,
    nodes: Vec<std::collections::BTreeSet<cairn_schema::NodeKey>>,
) -> Result<(), Abort> {
    let numbered = (first_position..).zip(&commit.change_set.events);
    for ((seq, event), nodes) in numbered.zip(nodes) {
        let Event {
            patch_id,
            ordinal,
            log,
            event_type,
            actor: Actor { user, agent },
            confirming_user,
            subject,
            at,
            note,
            delta,
        } = event;
        let (log_kind, log_id) = domain_columns(log);
        execute(
            connection,
            "INSERT INTO events (seq, patch_id, ordinal, log_kind, log_id, event_type, \
             actor_user, agent, confirming_user, subject, at, note, delta) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            vec![
                int(seq),
                text(patch_id),
                int(*ordinal),
                text(log_kind),
                log_id,
                json_enum(event_type)?,
                text(user),
                opt_text(agent.as_ref()),
                opt_text(confirming_user.as_ref()),
                json(subject)?,
                time(*at)?,
                opt_text(note.as_ref()),
                json(delta)?,
            ],
        )
        .await?;
        for node in nodes {
            execute(
                connection,
                "INSERT INTO event_nodes (seq, node) VALUES (?1, ?2)",
                vec![int(seq), text(&node)],
            )
            .await?;
        }
    }
    Ok(())
}

async fn store_receipt(connection: &Connection, stored: &StoredReceipt) -> Result<(), Abort> {
    let StoredReceipt {
        receipt:
            PatchReceipt {
                patch_id,
                domain,
                content_hash,
                revision,
            },
        proposal,
        deployment_revision,
        proposals,
        deployment_touched,
    } = stored;
    let (kind, id) = domain_columns(domain);
    execute(
        connection,
        "INSERT INTO patch_receipts (patch_id, domain_kind, domain_id, proposal_id, content_hash, \
         revision, deployment_revision, proposals, deployment_touched) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        vec![
            text(patch_id),
            text(kind),
            id,
            opt_text(proposal.as_ref()),
            text(content_hash.as_str()),
            int(revision.get()),
            opt_int(deployment_revision.map(Revision::get)),
            json(proposals)?,
            json(deployment_touched)?,
        ],
    )
    .await?;
    Ok(())
}

/// The answer after a second write-write conflict (ARCHITECTURE, Concurrency), which only a
/// connection outside this process's turns can cause: stale with every revision the commit
/// names that has moved and what intervened, read now. When none has, the commit it lost to
/// is still in flight and nothing it did can be seen, so the commit fails rather than name a
/// revision that may never exist with nothing intervening (H5;
/// decisions/2026-10-06-a-resubmission-beside-its-own-original-in-flight-is-answered.md).
async fn conflicted(connection: &Connection, commit: &Commit, shape: &Shape) -> CommitError {
    let mut conflicts = Vec::new();
    for (of, expected) in backend::expected_revisions(commit, shape) {
        let now = match load::revision_of(connection, &of).await {
            Ok(now) => now,
            Err(error) => return CommitError::Failed(error),
        };
        if now != expected {
            conflicts.push(RevisionConflict {
                of,
                expected,
                current: now,
            });
        }
    }
    if conflicts.is_empty() {
        return CommitError::Failed(StoreError::Backend(format!(
            "{} conflicted twice with a commit still in flight that took no turn",
            commit.target.domain()
        )));
    }
    match intervening(connection, &conflicts).await {
        Ok(intervening) => backend::stale(conflicts, intervening),
        Err(error) => CommitError::Failed(error),
    }
}
