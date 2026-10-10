//! Domain patches (A17; ARCHITECTURE, Service layer and composition): receipt first, load,
//! apply, consequences, commit, notify. The service never retries: a stale patch is answered
//! with the revisions that moved and what their events touched, and the client decides
//! (H5).

use std::collections::BTreeMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use cairn_engine::{Applied, ApplyInputs, Records, apply};
use cairn_schema::{
    Consequences, Domain, EventType, JourneyId, Location, Markdown, Mutation, Notice, Patch,
    PatchReceipt, PatchTarget, Record, Rejection, Revision, RevisionOf, Subject, Violation,
    ViolationCode, Violations, Write,
};
use cairn_store::{Commit, CommitError, Committed, EventQuery, PageSize, Store};

use crate::{Call, Service, ServiceError, consequence, load, notices};

/// A patch to a journey, a route with its draft, or the deployment (A17), with the note its
/// events carry (J1). A patch to a proposal goes through the proposal's own operations
/// (4.8), so it is not a domain patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DomainPatch {
    patch: Patch,
    note: Option<Markdown>,
}

impl DomainPatch {
    /// The patch, with an optional note for each of its events.
    ///
    /// # Errors
    ///
    /// [`NotADomainPatch`] when it targets a proposal.
    pub fn new(patch: Patch, note: Option<Markdown>) -> Result<Self, NotADomainPatch> {
        if let PatchTarget::Proposal { .. } = patch.target {
            return Err(NotADomainPatch);
        }
        Ok(Self { patch, note })
    }

    /// The patch.
    #[must_use]
    pub fn patch(&self) -> &Patch {
        &self.patch
    }
}

/// The patch targets a proposal, which is edited through its own lifecycle, not as a domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotADomainPatch;

impl fmt::Display for NotADomainPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a proposal is edited through its own operations, not as a domain")
    }
}

impl std::error::Error for NotADomainPatch {}

/// What an accepted domain patch answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    /// Committed now: the receipt, and what it newly caused in each journey it changed or
    /// whose meaning it changed (D7; a journey with nothing new is left out).
    Applied {
        /// The receipt: patch id, domain, content hash, and the revision produced.
        receipt: PatchReceipt,
        /// D7, by journey.
        consequences: BTreeMap<JourneyId, Consequences>,
        /// A20: the notices of the graph a route import or publish leaves; none for any
        /// other patch. Advisory: the patch was accepted.
        notices: Vec<Notice>,
    },
    /// The patch id was committed before with the same content (H5): answered from its
    /// receipt, with no consequences, which are reported only the first time.
    AlreadyApplied {
        /// The original receipt.
        receipt: PatchReceipt,
    },
}

impl Written {
    /// The receipt, whichever way it went.
    #[must_use]
    pub fn receipt(&self) -> &PatchReceipt {
        match self {
            Written::Applied { receipt, .. } | Written::AlreadyApplied { receipt } => receipt,
        }
    }
}

/// A domain patch that was not committed. Either way nothing was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The patch is stale (with what intervened), reuses a patch id, or is invalid (every
    /// violation): the engine's and the store's rejection shapes (A15, H5).
    Rejected(Rejection),
    /// The service or the store failed.
    Failed(ServiceError),
}

impl From<ServiceError> for WriteError {
    fn from(error: ServiceError) -> Self {
        WriteError::Failed(error)
    }
}

impl From<cairn_store::StoreError> for WriteError {
    fn from(error: cairn_store::StoreError) -> Self {
        WriteError::Failed(ServiceError::Store(error))
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Rejected(rejection) => write!(formatter, "rejected: {rejection:?}"),
            WriteError::Failed(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for WriteError {}

impl<S: Store> Service<S> {
    /// A17: applies a domain patch as `call`'s actor (H2), at `call`'s today in the
    /// deployment's zone, and commits it. In order: a patch id already committed is answered
    /// from its receipt before anything is loaded (H5); an upgrade or re-link, which changes
    /// a journey only once confirmed through its proposal, is refused (B7, B9, I7); the
    /// records it reads, including any proposal it applies, are loaded;
    /// `apply` validates it (A15, D4); its consequences are derived (D7); the commit rechecks
    /// every revision it read (H5, E6); and every revision the commit moved is announced
    /// (H6).
    ///
    /// # Errors
    ///
    /// [`WriteError::Rejected`] for a stale, reused, or invalid patch;
    /// [`WriteError::Failed`] when the store fails or the engine panics.
    pub async fn patch(&self, call: &Call, submitted: &DomainPatch) -> Result<Written, WriteError> {
        let patch = submitted.patch();
        if let Some(answer) = self.answer_from_receipt(patch).await? {
            patina_dst::reachable!("service-resubmission-answered-from-receipt");
            return answer;
        }
        if let Some(rejection) = unconfirmed(patch) {
            return Err(WriteError::Rejected(rejection));
        }
        self.write(call, patch, submitted.note.clone()).await
    }

    /// The write path after the receipt lookup, for a domain patch and a patch to a proposal
    /// alike: load, apply, consequences, commit, announce.
    pub(crate) async fn write(
        &self,
        call: &Call,
        patch: &Patch,
        note: Option<Markdown>,
    ) -> Result<Written, WriteError> {
        let today = self.settings.today(call.now);
        let loaded = load::records(&*self.store, patch).await?;
        let inputs = ApplyInputs {
            today,
            at: call.now,
            actor: call.actor.clone(),
            note,
        };
        let accepted = engine(&patch.target.domain(), || {
            apply(&loaded.records, patch, &inputs).map(|applied| {
                let caused = consequence::of(
                    &patch.target,
                    &loaded.records,
                    applied.records(),
                    today,
                    &self.settings,
                );
                let noticed = notices::of(patch, &loaded.records, &applied);
                (applied, caused, noticed)
            })
        })?;
        match accepted {
            Ok((applied, caused, noticed)) => {
                self.commit(patch, &loaded, &applied, (caused, noticed))
                    .await
            }
            Err(Rejection::Stale { conflicts, .. }) => {
                // H5: the same patch may have committed since the receipt was looked up, and
                // its own commit is what moved the revision; the receipt answers it then.
                if let Some(answer) = self.answer_from_receipt(patch).await? {
                    patina_dst::reachable!("service-duplicate-original-committed-meanwhile");
                    return answer;
                }
                patina_dst::reachable!("service-stale-patch-completed-with-intervening");
                let intervening = self.store.intervening(&conflicts).await?;
                Err(WriteError::Rejected(Rejection::Stale {
                    conflicts,
                    intervening,
                }))
            }
            Err(rejection) => Err(WriteError::Rejected(rejection)),
        }
    }

    /// H5: the answer for a patch id already committed: the receipt for the same content,
    /// a rejection for other content; none for an id never committed.
    pub(crate) async fn answer_from_receipt(
        &self,
        patch: &Patch,
    ) -> Result<Option<Result<Written, WriteError>>, WriteError> {
        let Some(receipt) = self.store.receipt(&patch.id).await? else {
            return Ok(None);
        };
        let same = receipt.content_hash == patch.content_hash() || self.merged_as(patch).await?;
        Ok(Some(if same {
            Ok(Written::AlreadyApplied { receipt })
        } else {
            Err(WriteError::Rejected(Rejection::PatchIdReused {
                patch_id: patch.id.clone(),
            }))
        }))
    }

    /// H5, E6: whether `patch`, a lone entity merge whose id is committed, asks for the
    /// merge that id committed. A merge names the journeys referencing either entity at
    /// their revisions when it was drafted, which a client reads from the index, so a
    /// resubmission after any of them moved carries other content for the same merge; it is
    /// matched by what it merges (briefs/4.3-mcp.md, Decision log): the committed patch's
    /// one event merged the same entity into the same survivor.
    async fn merged_as(&self, patch: &Patch) -> Result<bool, WriteError> {
        let [
            Mutation::MergeEntities {
                survivor, merged, ..
            },
        ] = patch.mutations.as_slice()
        else {
            return Ok(false);
        };
        if patch.target != PatchTarget::Deployment {
            return Ok(false);
        }
        let query = EventQuery {
            log: Some(Domain::Deployment),
            patch: Some(patch.id.clone()),
            size: PageSize::new(2),
            ..EventQuery::default()
        };
        let events = self.store.events(&query).await?.items;
        let [logged] = events.as_slice() else {
            return Ok(false);
        };
        let alias = Record::EntityAlias {
            alias: merged.clone(),
            entity: survivor.clone(),
        };
        let event = &logged.event;
        Ok(event.event_type == EventType::EntitiesMerged
            && event.subject == Subject::Entity(merged.clone())
            && event.delta.contains(&Write::Put(alias)))
    }

    /// Commits what `apply` accepted, rechecking every revision it read, and announces it.
    ///
    /// # Panics
    ///
    /// When the store answers with a receipt other than the change set's.
    async fn commit(
        &self,
        patch: &Patch,
        loaded: &load::Loaded,
        applied: &Applied,
        (caused, noticed): (BTreeMap<JourneyId, Consequences>, Vec<Notice>),
    ) -> Result<Written, WriteError> {
        let commit = Commit {
            target: patch.target.clone(),
            base_revision: patch.base_revision,
            preconditions: loaded.preconditions(patch, applied.records()),
            change_set: applied.change_set().clone(),
        };
        match self.store.commit(commit).await {
            Ok(Committed::Applied(receipt)) => {
                #[cfg(not(target_arch = "wasm32"))]
                metrics::counter!("cairn_write_commits_total").increment(1);
                assert_eq!(
                    receipt,
                    applied.change_set().receipt,
                    "the store keeps the receipt"
                );
                self.announce(&patch.target, &receipt, &loaded.records, applied)
                    .await;
                Ok(Written::Applied {
                    receipt,
                    consequences: caused,
                    notices: noticed,
                })
            }
            Ok(Committed::AlreadyApplied(receipt)) => {
                patina_dst::reachable!("service-resubmission-raced-original");
                Ok(Written::AlreadyApplied { receipt })
            }
            Err(CommitError::Rejected(rejection)) => {
                patina_dst::reachable!("service-patch-lost-at-commit");
                Err(WriteError::Rejected(rejection))
            }
            Err(CommitError::Failed(error)) => Err(error.into()),
        }
    }

    /// H6: announces every revision a commit moved: its domain's, or for a patch to a
    /// proposal the proposal's (I6: drafting never moves the destination); each proposal a
    /// domain patch applied; and the deployment's when an entity create riding in a journey or
    /// route patch moved it. The deployment's revision is read back from the store, which
    /// another such commit may have moved further in the meantime: announcing an older one
    /// would be ignored by a subscriber that heard the newer, and announcing one never
    /// committed would hide the real one when it comes.
    async fn announce(
        &self,
        target: &PatchTarget,
        receipt: &PatchReceipt,
        before: &Records,
        applied: &Applied,
    ) {
        let domain = receipt.domain.clone();
        if let PatchTarget::Proposal { id, .. } = target {
            self.notifier
                .publish(&RevisionOf::Proposal(id.clone()), receipt.revision);
            return;
        }
        self.notifier
            .publish(&RevisionOf::Domain(domain.clone()), receipt.revision);
        for (id, proposal) in &applied.records().proposals {
            if before.proposals.get(id).map(|held| held.revision) != Some(proposal.revision) {
                self.notifier
                    .publish(&RevisionOf::Proposal(id.clone()), proposal.revision);
            }
        }
        let creates_entities = applied.change_set().writes().any(|write| {
            matches!(
                write,
                Write::Put(Record::Entity(_) | Record::EntityAlias { .. })
            )
        });
        if domain == Domain::Deployment || !creates_entities {
            return;
        }
        // The store's revision, or, when it cannot be read back, the one the engine's records
        // computed: a riding create moves the deployment once per commit, so that is this
        // commit's revision or a lower one already committed, never one that does not exist.
        let computed = applied.records().deployment.revision;
        let committed = if let Ok(revisions) = self.store.revisions().await {
            revisions.deployment
        } else {
            patina_dst::reachable!("service-deployment-revision-unread");
            computed
        };
        if committed > before.deployment.revision {
            self.notifier
                .publish(&RevisionOf::Domain(Domain::Deployment), committed);
        }
    }
}

/// B7, B9, I7: an upgrade or a re-link changes a journey only once someone confirms it, by
/// applying the proposal that drafted it; submitted directly, each is refused, one violation
/// per mutation. A proposal's own mutations reach the engine through its apply, not here.
fn unconfirmed(patch: &Patch) -> Option<Rejection> {
    let found: Vec<Violation> = patch
        .mutations
        .as_slice()
        .iter()
        .enumerate()
        .filter(|(_, mutation)| {
            matches!(mutation, Mutation::Upgrade { .. } | Mutation::Relink { .. })
        })
        .map(|(position, _)| {
            violation(
                ViolationCode::MutationNotForTarget,
                Some(u32::try_from(position).unwrap_or(u32::MAX)),
                "an upgrade or re-link is applied by confirming its proposal (B7, B9)".to_owned(),
            )
        })
        .collect();
    Violations::new(found)
        .ok()
        .map(|violations| Rejection::Invalid { violations })
}

/// The revision before `revision`: the base a committed patch named, given the revision its
/// receipt says it produced (A17: each patch moves its target by one).
pub(crate) fn before(revision: Revision) -> Revision {
    Revision::try_from(revision.get().saturating_sub(1)).unwrap_or(Revision::NONE)
}

/// A violation the service finds itself, before the engine sees the patch (A15: the same
/// shape as the engine's).
pub(crate) fn violation(code: ViolationCode, mutation: Option<u32>, message: String) -> Violation {
    Violation {
        code,
        at: Location {
            mutation,
            ..Location::default()
        },
        message,
        related: Vec::new(),
        limit: None,
        bypassable: None,
        failures: std::collections::BTreeSet::new(),
        chains: None,
        caused_by: std::collections::BTreeSet::new(),
    }
}

/// Runs an engine call, catching a panic (PRACTICES, Programmer errors panic: the engine
/// works on a private candidate and nothing commits, so the request fails and the host logs
/// it with the domain).
pub(crate) fn engine<T>(domain: &Domain, call: impl FnOnce() -> T) -> Result<T, ServiceError> {
    catch_unwind(AssertUnwindSafe(|| {
        let _inside = crate::observe::EngineCall::enter();
        call()
    }))
    .map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_owned());
        ServiceError::EnginePanic {
            domain: domain.clone(),
            message,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_panic_becomes_an_error_naming_the_domain() {
        let domain = Domain::Journey("j_one".parse().unwrap());
        let caught = engine(&domain, || -> u32 { panic!("an engine assertion failed") });
        assert_eq!(
            caught,
            Err(ServiceError::EnginePanic {
                domain: domain.clone(),
                message: "an engine assertion failed".to_owned(),
            })
        );
        assert_eq!(engine(&domain, || 7), Ok(7));
    }
}
