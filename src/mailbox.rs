//! Durable, server-owned mailbox domain state for the `mailbox.v1` follow-on.
//!
//! This slice deliberately contains no API, recipient discovery, authorization, Pi,
//! transport, or notification code. A later slice may notify a recipient only after
//! `claim` has returned from this durable store.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const RECORD_STREAM_FILE: &str = "mailbox.v1.jsonl";
pub const LOCK_FILE: &str = "mailbox.v1.lock";

/// Durable mailbox ownership. It is intentionally independent of Pi session files.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct RecipientKey {
    pub recipient_id: String,
    pub generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailboxHead {
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub delivery_digest: String,
    pub recipient: RecipientKey,
    pub subject: String,
    pub body: String,
    pub recipient_generation: String,
    pub sender: String,
    pub target: String,
    pub grant_id: String,
    pub message_id: String,
    pub kind: String,
    pub priority: String,
    pub original_sequence: u64,
    pub enqueue_epoch: u64,
    pub accepted_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxProvenance {
    pub sender: String,
    pub target: String,
    pub grant_id: String,
    pub accepted_at: u64,
}

/// Exact server-side compare-and-swap input for the one editable, unclaimed
/// head. The caller supplies only its observed version and human text intent;
/// the store mints the next revision and digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxHeadEdit {
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxHeadEditRecord {
    expected_stable_id: String,
    expected_revision: u64,
    expected_digest: String,
    head: MailboxHead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReceipt {
    pub delivery_digest: String,
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub status: ReceiptStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Admitted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub claim_id: String,
    pub recipient: RecipientKey,
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimResolutionOutcome {
    Admitted,
    Settled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimResolution {
    pub claim_id: String,
    pub outcome: ClaimResolutionOutcome,
}

/// Durable server-owned recipient policy. This is deliberately distinct from
/// a transient sender/consumer execution binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailboxGrant {
    pub grant_id: String,
    pub sender: RecipientKey,
    pub recipient: RecipientKey,
}

/// Append-only stream. Records themselves are immutable; recovered state is a projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailboxRecord {
    Head {
        head: MailboxHead,
    },
    HeadEdit {
        edit: MailboxHeadEditRecord,
    },
    Receipt {
        receipt: AdmissionReceipt,
    },
    Claim {
        claim: Claim,
    },
    Resolution {
        resolution: ClaimResolution,
    },
    Grant {
        grant: MailboxGrant,
    },
    ChildReport {
        event: crate::child_report::ChildReportEvent,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveredMailbox {
    pub heads: BTreeMap<String, MailboxHead>,
    pub receipts: BTreeMap<String, AdmissionReceipt>,
    pub claims: BTreeMap<String, Claim>,
    pub resolutions: BTreeMap<String, ClaimResolution>,
    pub grants: BTreeMap<String, MailboxGrant>,
    /// Full identity and provider coverage are required before a missing status.
    pub child_report_events: Vec<crate::child_report::ChildReportEvent>,
    /// Count of durably replayed records, including other mailbox events.
    pub record_cursor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxError {
    InvalidRecord,
    ConflictingDuplicate,
    MissingHead,
    ClaimAlreadyExists,
    ClaimAlreadyResolved,
    MissingClaim,
    EditConflict,
    HeadClaimed,
    Io(String),
    CorruptRecord,
}

impl std::fmt::Display for MailboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for MailboxError {}
impl From<std::io::Error> for MailboxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug)]
pub struct MailboxStore {
    stream_path: PathBuf,
    lock_path: PathBuf,
}

impl MailboxStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, MailboxError> {
        std::fs::create_dir_all(directory.as_ref())?;
        Ok(Self::existing(directory))
    }

    /// Constructor for a read-only projection. Unlike `open`, even a missing
    /// directory is never created by a parent disposition query.
    pub fn existing(directory: impl AsRef<Path>) -> Self {
        Self {
            stream_path: directory.as_ref().join(RECORD_STREAM_FILE),
            lock_path: directory.as_ref().join(LOCK_FILE),
        }
    }

    pub fn load(&self) -> Result<RecoveredMailbox, MailboxError> {
        let stream = match File::open(&self.stream_path) {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RecoveredMailbox::default())
            }
            Err(error) => return Err(error.into()),
        };
        let mut recovered = RecoveredMailbox::default();
        for line in BufReader::new(stream).lines() {
            let record = serde_json::from_str(&line?).map_err(|_| MailboxError::CorruptRecord)?;
            recovered.apply(record)?;
        }
        Ok(recovered)
    }

    /// Additive child-local Todo/report observation; only authenticated App
    /// dispatch may call this. The Pi producer is a separate required contract.
    /// The return cursor is durable and stable across recovery, not an ACK to Pi.
    pub fn append_child_report_event(
        &self,
        mut event: crate::child_report::ChildReportEvent,
    ) -> Result<u64, MailboxError> {
        self.with_exclusive_lock(|| {
            let recovered = self.load()?;
            // No server producer has proved exhaustive delivery-path closure.
            // In particular, even an internal caller cannot promote a child's
            // observed-only ACK or Todo assertion into authoritative coverage.
            if matches!(&event, crate::child_report::ChildReportEvent::CoverageBarrier {
                qualification: crate::child_report::CoverageQualification::AllPathsTrusted, ..
            }) {
                return Err(MailboxError::InvalidRecord);
            }
            if let crate::child_report::ChildReportEvent::CoverageBarrier {
                route, local_root, local_revision, qualification, ..
            } = &event {
                if let Some(earlier) = recovered.child_report_events.iter().find_map(|prior| match prior {
                    crate::child_report::ChildReportEvent::CoverageBarrier {
                        route: prior_route, local_root: root, local_revision: revision,
                        qualification: prior_qualification, ..
                    } if prior_route == route && root == local_root && revision == local_revision =>
                        Some(prior_qualification),
                    _ => None,
                }) {
                    return if earlier == qualification { Ok(recovered.record_cursor) }
                        else { Err(MailboxError::ConflictingDuplicate) };
                }
                if recovered.child_report_events.iter().any(|prior| {
                    prior.route() == route && matches!(prior,
                        crate::child_report::ChildReportEvent::Attempt { .. }
                            | crate::child_report::ChildReportEvent::Coverage { .. }
                            | crate::child_report::ChildReportEvent::Bypass { .. })
                }) || recovered.heads.values().any(|head| {
                    head.kind == "report" && head.sender == route.child_terminal_id
                        && head.target == route.parent_terminal_id
                        && !recovered.child_report_events.iter().any(|prior| matches!(prior,
                            crate::child_report::ChildReportEvent::PreparedAttempt { preparation }
                                if &preparation.route == route && preparation.delivery_digest == head.delivery_digest))
                }) {
                    return Err(MailboxError::InvalidRecord);
                }
            }
            if let crate::child_report::ChildReportEvent::CoverageBarrier { through_cursor, .. } = &mut event {
                *through_cursor = recovered.record_cursor;
            }
            match crate::child_report::validate_next(&recovered.child_report_events, &event) {
                Ok(false) => return Ok(recovered.record_cursor),
                Err(()) => return Err(MailboxError::ConflictingDuplicate),
                Ok(true) => {}
            }
            self.append_synced(&MailboxRecord::ChildReport { event })?;
            recovered
                .record_cursor
                .checked_add(1)
                .ok_or(MailboxError::InvalidRecord)
        })
    }

    pub fn provision_grant(&self, grant: MailboxGrant) -> Result<(), MailboxError> {
        self.with_exclusive_lock(|| {
            if grant.grant_id.is_empty() || grant.sender == grant.recipient {
                return Err(MailboxError::InvalidRecord);
            }
            let recovered = self.load()?;
            if let Some(existing) = recovered.grants.get(&grant.grant_id) {
                return if existing == &grant {
                    Ok(())
                } else {
                    Err(MailboxError::ConflictingDuplicate)
                };
            }
            self.append_synced(&MailboxRecord::Grant { grant })
        })
    }

    /// Offline sender admission: the server syncs the immutable head, then mints and syncs
    /// the exact receipt. Clients never provide the accepted receipt fields.
    pub fn append_offline_head(
        &self,
        mut head: MailboxHead,
    ) -> Result<AdmissionReceipt, MailboxError> {
        self.with_exclusive_lock(|| {
            let recovered = self.load()?;
            head.enqueue_epoch = recovered
                .heads
                .values()
                .map(|existing| existing.enqueue_epoch)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(MailboxError::InvalidRecord)?;
            validate_head(&head)?;
            if head.grant_id.starts_with("bound-parent-report:")
                && !recovered.child_report_events.iter().any(|event| {
                    matches!(event,
                        crate::child_report::ChildReportEvent::PreparedAttempt { preparation }
                            if crate::child_report::matches_prepared_head(preparation,&head)
                    )
                })
            {
                return Err(MailboxError::InvalidRecord);
            }
            if let Some(existing) = recovered.heads.get(&head.stable_id) {
                if existing != &head {
                    return Err(MailboxError::ConflictingDuplicate);
                }
                return recovered
                    .receipts
                    .get(&head.delivery_digest)
                    .cloned()
                    .ok_or(MailboxError::CorruptRecord);
            }
            self.append_synced(&MailboxRecord::Head { head: head.clone() })?;
            // `append_synced` above completed `sync_all`; mint only after that durable head.
            let receipt = AdmissionReceipt {
                delivery_digest: head.delivery_digest.clone(),
                stable_id: head.stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                status: ReceiptStatus::Admitted,
            };
            self.append_synced(&MailboxRecord::Receipt {
                receipt: receipt.clone(),
            })?;
            Ok(receipt)
        })
    }

    /// Appends the head and forces it to stable storage before appending the exact receipt.
    /// A caller must not treat either return value as a Pi or model acknowledgement.
    pub fn append_head_and_receipt(
        &self,
        head: MailboxHead,
        receipt: AdmissionReceipt,
    ) -> Result<(), MailboxError> {
        self.with_exclusive_lock(|| {
            validate_head(&head)?;
            validate_receipt(&receipt)?;
            if receipt.delivery_digest != head.delivery_digest
                || receipt.stable_id != head.stable_id
                || receipt.revision != head.revision
                || receipt.digest != head.digest
            {
                return Err(MailboxError::InvalidRecord);
            }
            let recovered = self.load()?;
            if let Some(existing) = recovered.heads.get(&head.stable_id) {
                if existing != &head {
                    return Err(MailboxError::ConflictingDuplicate);
                }
            }
            if let Some(existing) = recovered.receipts.get(&receipt.delivery_digest) {
                return if existing == &receipt {
                    Ok(())
                } else {
                    Err(MailboxError::ConflictingDuplicate)
                };
            }
            if !recovered.heads.contains_key(&head.stable_id) {
                self.append_synced(&MailboxRecord::Head { head })?;
            }
            // The preceding append_synced performed File::sync_all before this receipt is written.
            self.append_synced(&MailboxRecord::Receipt { receipt })
        })
    }

    /// Atomically replaces only an unclaimed head whose complete observed version
    /// matches. The append is fsynced before the refreshed authoritative head is
    /// returned. Sender, recipient, grant, message and delivery provenance remain
    /// immutable; only the human subject/body and server-minted version change.
    pub fn edit_unclaimed_head(&self, edit: MailboxHeadEdit) -> Result<MailboxHead, MailboxError> {
        self.with_exclusive_lock(|| {
            if edit.stable_id.is_empty()
                || edit.revision == 0
                || !valid_digest(&edit.digest)
                || edit.subject.is_empty()
                || edit.body.is_empty()
            {
                return Err(MailboxError::InvalidRecord);
            }
            let recovered = self.load()?;
            let current = recovered
                .heads
                .get(&edit.stable_id)
                .cloned()
                .ok_or(MailboxError::EditConflict)?;
            if current.revision != edit.revision || current.digest != edit.digest {
                return Err(MailboxError::EditConflict);
            }
            if recovered.claims.contains_key(&edit.stable_id) {
                return Err(MailboxError::HeadClaimed);
            }
            let revision = current
                .revision
                .checked_add(1)
                .ok_or(MailboxError::InvalidRecord)?;
            let digest = edit_digest(
                &current.stable_id,
                revision,
                &current.digest,
                &edit.subject,
                &edit.body,
            );
            let next = MailboxHead {
                revision,
                digest,
                subject: edit.subject,
                body: edit.body,
                ..current.clone()
            };
            validate_head(&next)?;
            let record = MailboxHeadEditRecord {
                expected_stable_id: current.stable_id.clone(),
                expected_revision: current.revision,
                expected_digest: current.digest,
                head: next.clone(),
            };
            self.append_synced(&MailboxRecord::HeadEdit { edit: record })?;
            self.load()?
                .heads
                .get(&next.stable_id)
                .cloned()
                .ok_or(MailboxError::CorruptRecord)
        })
    }

    /// The claim record is forced before this method returns. Notification is intentionally
    /// absent: a crash after this call recovers an outstanding claim instead of resending.
    pub fn claim(&self, claim: Claim) -> Result<(), MailboxError> {
        self.with_exclusive_lock(|| {
            validate_claim(&claim)?;
            let recovered = self.load()?;
            let head = recovered
                .heads
                .get(&claim.stable_id)
                .ok_or(MailboxError::MissingHead)?;
            if head.recipient != claim.recipient
                || head.revision != claim.revision
                || head.digest != claim.digest
            {
                return Err(MailboxError::InvalidRecord);
            }
            if let Some(existing) = recovered.claims.get(&claim.stable_id) {
                return if existing == &claim {
                    Ok(())
                } else {
                    Err(MailboxError::ClaimAlreadyExists)
                };
            }
            self.append_synced(&MailboxRecord::Claim { claim })
        })
    }

    /// Returns the one outstanding claim for a recipient, or atomically claims
    /// its next unclaimed head. This prevents a replay from creating a second
    /// consumer execution record.
    pub fn claim_next(&self, recipient: &RecipientKey) -> Result<Option<Claim>, MailboxError> {
        self.with_exclusive_lock(|| {
            let recovered = self.load()?;
            if let Some(existing) = recovered
                .claims
                .values()
                .find(|claim| {
                    &claim.recipient == recipient
                        && !matches!(
                            recovered.resolutions.get(&claim.claim_id),
                            Some(ClaimResolution {
                                outcome: ClaimResolutionOutcome::Settled,
                                ..
                            })
                        )
                })
                .cloned()
            {
                return Ok(Some(existing));
            }
            let Some(head) = recovered
                .heads
                .values()
                .filter(|head| {
                    &head.recipient == recipient && !recovered.claims.contains_key(&head.stable_id)
                })
                // Priority wins; within a tier use the server-minted enqueue epoch,
                // never caller-provided sequence or the stable-ID map ordering.
                .min_by_key(|head| {
                    let priority = match head.priority.as_str() {
                        "high" => 0,
                        "normal" => 1,
                        "low" => 2,
                        _ => 3,
                    };
                    (priority, head.enqueue_epoch, &head.stable_id)
                })
            else {
                return Ok(None);
            };
            let claim = Claim {
                claim_id: format!(
                    "claim:{}:{}:{}",
                    recipient.recipient_id, recipient.generation, head.stable_id
                ),
                recipient: recipient.clone(),
                stable_id: head.stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
            };
            self.append_synced(&MailboxRecord::Claim {
                claim: claim.clone(),
            })?;
            Ok(Some(claim))
        })
    }

    /// Persists admission followed by settlement. Admission remains outstanding;
    /// identical retries do not append, and settlement cannot regress to admission.
    pub fn resolve_claim(
        &self,
        claim_id: &str,
        outcome: ClaimResolutionOutcome,
    ) -> Result<ClaimResolution, MailboxError> {
        self.with_exclusive_lock(|| {
            let recovered = self.load()?;
            if !recovered
                .claims
                .values()
                .any(|claim| claim.claim_id == claim_id)
            {
                return Err(MailboxError::MissingClaim);
            }
            let resolution = ClaimResolution {
                claim_id: claim_id.into(),
                outcome,
            };
            if let Some(existing) = recovered.resolutions.get(claim_id) {
                if existing == &resolution {
                    return Ok(existing.clone());
                }
                if existing.outcome != ClaimResolutionOutcome::Admitted
                    || outcome != ClaimResolutionOutcome::Settled
                {
                    return Err(MailboxError::ClaimAlreadyResolved);
                }
            }
            self.append_synced(&MailboxRecord::Resolution {
                resolution: resolution.clone(),
            })?;
            Ok(resolution)
        })
    }

    fn append_synced(&self, record: &MailboxRecord) -> Result<(), MailboxError> {
        let newly_created = !self.stream_path.exists();
        let mut stream = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.stream_path)?;
        serde_json::to_writer(&mut stream, record)
            .map_err(|error| MailboxError::Io(error.to_string()))?;
        stream.write_all(b"\n")?;
        stream.sync_all()?;
        if newly_created {
            File::open(
                self.stream_path
                    .parent()
                    .ok_or(MailboxError::InvalidRecord)?,
            )?
            .sync_all()?;
        }
        Ok(())
    }

    fn with_exclusive_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, MailboxError>,
    ) -> Result<T, MailboxError> {
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        #[cfg(unix)]
        unsafe {
            use std::os::fd::AsRawFd;
            if libc::flock(lock.as_raw_fd(), libc::LOCK_EX) != 0 {
                return Err(MailboxError::Io(
                    std::io::Error::last_os_error().to_string(),
                ));
            }
        }
        if !self.owns_exclusive_lock(&lock) {
            return Err(MailboxError::Io("mailbox writer lease was replaced".into()));
        }
        let result = operation();
        let still_owned = self.owns_exclusive_lock(&lock);
        #[cfg(unix)]
        unsafe {
            use std::os::fd::AsRawFd;
            libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
        }
        if !still_owned {
            return Err(MailboxError::Io("mailbox writer lease was replaced".into()));
        }
        result
    }

    #[cfg(unix)]
    fn owns_exclusive_lock(&self, lock: &File) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(opened) = lock.metadata() else {
            return false;
        };
        let Ok(path) = std::fs::symlink_metadata(&self.lock_path) else {
            return false;
        };
        opened.is_file()
            && path.is_file()
            && opened.dev() == path.dev()
            && opened.ino() == path.ino()
            && opened.uid() == unsafe { libc::geteuid() }
            && opened.nlink() == 1
    }

    #[cfg(not(unix))]
    fn owns_exclusive_lock(&self, _lock: &File) -> bool {
        // Preserve the existing mailbox policy; #119 gates trusted Pi route
        // identity independently and this additive feature cannot bypass it.
        true
    }
}

impl RecoveredMailbox {
    fn apply(&mut self, record: MailboxRecord) -> Result<(), MailboxError> {
        let result = match record {
            MailboxRecord::Head { head } => {
                insert_exact(&mut self.heads, head.stable_id.clone(), head)
            }
            MailboxRecord::HeadEdit { edit } => self.apply_head_edit(edit),
            MailboxRecord::Receipt { receipt } => {
                insert_exact(&mut self.receipts, receipt.delivery_digest.clone(), receipt)
            }
            MailboxRecord::Claim { claim } => {
                insert_exact(&mut self.claims, claim.stable_id.clone(), claim)
            }
            MailboxRecord::Resolution { resolution } => {
                if !self
                    .claims
                    .values()
                    .any(|claim| claim.claim_id == resolution.claim_id)
                {
                    return Err(MailboxError::CorruptRecord);
                }
                match self.resolutions.get(&resolution.claim_id) {
                    Some(existing) if existing == &resolution => Ok(()),
                    Some(existing)
                        if existing.outcome == ClaimResolutionOutcome::Admitted
                            && resolution.outcome == ClaimResolutionOutcome::Settled =>
                    {
                        self.resolutions
                            .insert(resolution.claim_id.clone(), resolution);
                        Ok(())
                    }
                    Some(_) => Err(MailboxError::ConflictingDuplicate),
                    None => {
                        self.resolutions
                            .insert(resolution.claim_id.clone(), resolution);
                        Ok(())
                    }
                }
            }
            MailboxRecord::Grant { grant } => {
                insert_exact(&mut self.grants, grant.grant_id.clone(), grant)
            }
            MailboxRecord::ChildReport { event } => {
                match crate::child_report::validate_next(&self.child_report_events, &event) {
                    Ok(true) => {
                        self.child_report_events.push(event);
                        Ok(())
                    }
                    Ok(false) | Err(()) => Err(MailboxError::CorruptRecord),
                }
            }
        };
        result?;
        self.record_cursor = self
            .record_cursor
            .checked_add(1)
            .ok_or(MailboxError::CorruptRecord)?;
        Ok(())
    }

    fn apply_head_edit(&mut self, edit: MailboxHeadEditRecord) -> Result<(), MailboxError> {
        let current = self
            .heads
            .get(&edit.expected_stable_id)
            .cloned()
            .ok_or(MailboxError::CorruptRecord)?;
        if current.revision != edit.expected_revision
            || current.digest != edit.expected_digest
            || self.claims.contains_key(&edit.expected_stable_id)
            || edit.head.stable_id != current.stable_id
            || edit.head.revision
                != current
                    .revision
                    .checked_add(1)
                    .ok_or(MailboxError::CorruptRecord)?
            || edit.head.delivery_digest != current.delivery_digest
            || edit.head.recipient != current.recipient
            || edit.head.recipient_generation != current.recipient_generation
            || edit.head.sender != current.sender
            || edit.head.target != current.target
            || edit.head.grant_id != current.grant_id
            || edit.head.message_id != current.message_id
            || edit.head.kind != current.kind
            || edit.head.priority != current.priority
            || edit.head.original_sequence != current.original_sequence
            || edit.head.enqueue_epoch != current.enqueue_epoch
            || edit.head.accepted_at != current.accepted_at
            || edit.head.subject.is_empty()
            || edit.head.body.is_empty()
            || edit.head.digest
                != edit_digest(
                    &current.stable_id,
                    edit.head.revision,
                    &current.digest,
                    &edit.head.subject,
                    &edit.head.body,
                )
        {
            return Err(MailboxError::CorruptRecord);
        }
        self.heads.insert(edit.head.stable_id.clone(), edit.head);
        Ok(())
    }
}

fn edit_digest(
    stable_id: &str,
    revision: u64,
    previous_digest: &str,
    subject: &str,
    body: &str,
) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for value in [
        stable_id.as_bytes(),
        &revision.to_be_bytes(),
        previous_digest.as_bytes(),
        subject.as_bytes(),
        body.as_bytes(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    format!("{:x}", hasher.finalize())
}

fn insert_exact<T: PartialEq>(
    map: &mut BTreeMap<String, T>,
    key: String,
    value: T,
) -> Result<(), MailboxError> {
    match map.get(&key) {
        Some(existing) if existing == &value => Ok(()),
        Some(_) => Err(MailboxError::ConflictingDuplicate),
        None => {
            map.insert(key, value);
            Ok(())
        }
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn validate_head(head: &MailboxHead) -> Result<(), MailboxError> {
    if head.stable_id.is_empty()
        || head.revision == 0
        || !valid_digest(&head.digest)
        || !valid_digest(&head.delivery_digest)
        || head.recipient.recipient_id.is_empty()
        || head.recipient.generation.is_empty()
        || head.recipient_generation != head.recipient.generation
        || head.sender.is_empty()
        || head.target.is_empty()
        || head.grant_id.is_empty()
        || head.message_id.is_empty()
        || head.kind.is_empty()
        || head.priority.is_empty()
        || head.original_sequence == 0
        || head.enqueue_epoch == 0
        || head.accepted_at == 0
    {
        return Err(MailboxError::InvalidRecord);
    }
    Ok(())
}
fn validate_receipt(receipt: &AdmissionReceipt) -> Result<(), MailboxError> {
    if receipt.stable_id.is_empty()
        || receipt.revision == 0
        || !valid_digest(&receipt.digest)
        || !valid_digest(&receipt.delivery_digest)
    {
        return Err(MailboxError::InvalidRecord);
    }
    Ok(())
}
fn validate_claim(claim: &Claim) -> Result<(), MailboxError> {
    if claim.claim_id.is_empty()
        || claim.stable_id.is_empty()
        || claim.revision == 0
        || !valid_digest(&claim.digest)
        || claim.recipient.recipient_id.is_empty()
        || claim.recipient.generation.is_empty()
    {
        return Err(MailboxError::InvalidRecord);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temporary_store() -> MailboxStore {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-mailbox-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        MailboxStore::open(path).unwrap()
    }
    #[test]
    fn read_only_existing_store_does_not_create_an_absent_directory() {
        let path = std::env::temp_dir().join(format!(
            "herdr-129-read-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!path.exists());
        let store = MailboxStore::existing(&path);
        assert_eq!(store.load().unwrap(), RecoveredMailbox::default());
        assert!(!path.exists());
    }

    #[test]
    fn bound_parent_head_cannot_bypass_the_prepared_attempt_gate() {
        let store = temporary_store();
        let mut head = head();
        head.grant_id = "bound-parent-report:unprepared".into();
        assert_eq!(
            store.append_offline_head(head),
            Err(MailboxError::InvalidRecord)
        );
        assert!(store.load().unwrap().heads.is_empty());
        std::fs::remove_dir_all(store.lock_path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replaced_mailbox_lock_never_acknowledges_an_exclusive_operation() {
        let store = temporary_store();
        let result: Result<(), MailboxError> = store.with_exclusive_lock(|| {
            let old = store.lock_path.with_extension("old");
            std::fs::rename(&store.lock_path, old)?;
            let _replacement = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&store.lock_path)?;
            Ok(())
        });
        assert!(matches!(result, Err(MailboxError::Io(_))));
        std::fs::remove_dir_all(store.lock_path.parent().unwrap()).unwrap();
    }

    fn head() -> MailboxHead {
        MailboxHead {
            stable_id: "recipient\0sender\0message".into(),
            revision: 1,
            digest: "a".repeat(64),
            delivery_digest: "b".repeat(64),
            recipient: RecipientKey {
                recipient_id: "recipient".into(),
                generation: "generation-1".into(),
            },
            subject: "subject".into(),
            body: "body".into(),
            recipient_generation: "generation-1".into(),
            sender: "sender".into(),
            target: "recipient".into(),
            grant_id: "grant".into(),
            message_id: "message".into(),
            kind: "report".into(),
            priority: "normal".into(),
            original_sequence: 1,
            enqueue_epoch: 1,
            accepted_at: 1,
        }
    }
    fn receipt(head: &MailboxHead) -> AdmissionReceipt {
        AdmissionReceipt {
            delivery_digest: head.delivery_digest.clone(),
            stable_id: head.stable_id.clone(),
            revision: head.revision,
            digest: head.digest.clone(),
            status: ReceiptStatus::Admitted,
        }
    }

    #[test]
    fn fsync_committed_head_precedes_postcommit_receipt() {
        let store = temporary_store();
        let head = head();
        store
            .append_head_and_receipt(head.clone(), receipt(&head))
            .unwrap();
        let lines: Vec<MailboxRecord> = BufReader::new(File::open(&store.stream_path).unwrap())
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect();
        assert!(matches!(
            lines.as_slice(),
            [MailboxRecord::Head { .. }, MailboxRecord::Receipt { .. }]
        ));
        assert_eq!(
            store.load().unwrap().receipts[&head.delivery_digest].status,
            ReceiptStatus::Admitted
        );
    }

    #[test]
    fn exact_receipt_is_idempotent_and_conflict_fails_closed() {
        let store = temporary_store();
        let head = head();
        let exact_receipt = receipt(&head);
        store
            .append_head_and_receipt(head.clone(), exact_receipt.clone())
            .unwrap();
        store
            .append_head_and_receipt(head.clone(), exact_receipt)
            .unwrap();
        let conflicting = MailboxHead {
            subject: "other".into(),
            ..head.clone()
        };
        assert_eq!(
            store.append_head_and_receipt(conflicting, receipt(&head)),
            Err(MailboxError::ConflictingDuplicate)
        );
    }

    #[test]
    fn admitted_claim_stays_outstanding_until_settled_then_next_claim_is_minted() {
        let store = temporary_store();
        let mut high = head();
        high.stable_id = "a-high".into();
        high.priority = "high".into();
        let mut normal = head();
        normal.stable_id = "z-normal".into();
        normal.delivery_digest = "c".repeat(64);
        store.append_offline_head(high.clone()).unwrap();
        store.append_offline_head(normal.clone()).unwrap();
        let claim = store.claim_next(&high.recipient).unwrap().unwrap();
        assert_eq!(claim.stable_id, high.stable_id);
        let admitted = store
            .resolve_claim(&claim.claim_id, ClaimResolutionOutcome::Admitted)
            .unwrap();
        let lines_before_retry = std::fs::read_to_string(&store.stream_path)
            .unwrap()
            .lines()
            .count();
        assert_eq!(
            store
                .resolve_claim(&claim.claim_id, ClaimResolutionOutcome::Admitted)
                .unwrap(),
            admitted
        );
        assert_eq!(
            std::fs::read_to_string(&store.stream_path)
                .unwrap()
                .lines()
                .count(),
            lines_before_retry
        );
        let reopened = MailboxStore::open(store.stream_path.parent().unwrap()).unwrap();
        assert_eq!(
            reopened.load().unwrap().resolutions[&claim.claim_id],
            admitted
        );
        assert_eq!(
            reopened.claim_next(&high.recipient).unwrap(),
            Some(claim.clone())
        );
        assert_eq!(reopened.load().unwrap().claims.len(), 1);
        let settled = reopened
            .resolve_claim(&claim.claim_id, ClaimResolutionOutcome::Settled)
            .unwrap();
        assert_eq!(settled.outcome, ClaimResolutionOutcome::Settled);
        let lines_after_settle = std::fs::read_to_string(&store.stream_path)
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines_after_settle, lines_before_retry + 1);
        assert_eq!(
            reopened
                .resolve_claim(&claim.claim_id, ClaimResolutionOutcome::Settled)
                .unwrap(),
            settled
        );
        assert_eq!(
            std::fs::read_to_string(&store.stream_path)
                .unwrap()
                .lines()
                .count(),
            lines_after_settle
        );
        assert_eq!(
            reopened.resolve_claim(&claim.claim_id, ClaimResolutionOutcome::Admitted),
            Err(MailboxError::ClaimAlreadyResolved)
        );
        let after_reload = MailboxStore::open(store.stream_path.parent().unwrap()).unwrap();
        assert_eq!(
            after_reload.load().unwrap().resolutions[&claim.claim_id],
            settled
        );
        let next = after_reload.claim_next(&high.recipient).unwrap().unwrap();
        assert_eq!(next.stable_id, normal.stable_id);
        assert_eq!(
            after_reload.claim_next(&high.recipient).unwrap(),
            Some(next)
        );
    }

    #[test]
    fn claim_next_prioritizes_high_then_server_enqueue_fifo_not_lexical_or_sender_sequence() {
        let store = temporary_store();
        let recipient = head().recipient;
        for (stable_id, priority, original_sequence, delivery_digest) in [
            ("y-normal-earlier", "normal", 99, "b"),
            ("a-normal-later", "normal", 1, "c"),
            ("z-high", "high", 2, "d"),
        ] {
            let mut item = head();
            item.stable_id = stable_id.into();
            item.priority = priority.into();
            item.original_sequence = original_sequence;
            item.delivery_digest = delivery_digest.repeat(64);
            store.append_offline_head(item).unwrap();
        }
        let high = store.claim_next(&recipient).unwrap().unwrap();
        assert_eq!(high.stable_id, "z-high");
        store
            .resolve_claim(&high.claim_id, ClaimResolutionOutcome::Settled)
            .unwrap();
        let first_normal = store.claim_next(&recipient).unwrap().unwrap();
        assert_eq!(first_normal.stable_id, "y-normal-earlier");
        store
            .resolve_claim(&first_normal.claim_id, ClaimResolutionOutcome::Settled)
            .unwrap();
        let second_normal = store.claim_next(&recipient).unwrap().unwrap();
        assert_eq!(second_normal.stable_id, "a-normal-later");
        store
            .resolve_claim(&second_normal.claim_id, ClaimResolutionOutcome::Settled)
            .unwrap();
        assert_eq!(store.claim_next(&recipient).unwrap(), None);
    }

    #[test]
    fn snapshot_distinguishes_fresh_admitted_settled_and_new_held_after_reload() {
        use crate::mailbox_v1::{snapshot, HeadLifecycle};
        let store = temporary_store();
        let old = head();
        let recipient = old.recipient.clone();
        store.append_offline_head(old.clone()).unwrap();
        let fresh = snapshot(&store.load().unwrap(), &recipient).unwrap();
        assert_eq!(fresh.head_states.len(), 1);
        assert_eq!(fresh.head_states[0].lifecycle, HeadLifecycle::Held);
        assert!(fresh.head_states[0].claim_id.is_none());
        assert!(fresh.claim.is_none());
        let claimed = store.claim_next(&recipient).unwrap().unwrap();
        let claimed_snapshot = snapshot(&store.load().unwrap(), &recipient).unwrap();
        assert_eq!(
            claimed_snapshot.head_states[0].lifecycle,
            HeadLifecycle::Claimed
        );
        assert_eq!(claimed_snapshot.claim, Some(claimed.clone()));
        store
            .resolve_claim(&claimed.claim_id, ClaimResolutionOutcome::Admitted)
            .unwrap();
        let admitted = snapshot(&store.load().unwrap(), &recipient).unwrap();
        assert_eq!(admitted.head_states[0].lifecycle, HeadLifecycle::Admitted);
        assert_eq!(admitted.claim, Some(claimed.clone()));
        store
            .resolve_claim(&claimed.claim_id, ClaimResolutionOutcome::Settled)
            .unwrap();
        let reopened = MailboxStore::open(store.stream_path.parent().unwrap()).unwrap();
        let settled = snapshot(&reopened.load().unwrap(), &recipient).unwrap();
        assert_eq!(settled.head_states[0].lifecycle, HeadLifecycle::Settled);
        assert_eq!(
            settled.head_states[0].claim_id.as_deref(),
            Some(claimed.claim_id.as_str())
        );
        assert!(settled.claim.is_none());
        assert_eq!(settled.heads.len(), 1);
        assert_eq!(settled.receipts.len(), 1);
        let mut new = head();
        new.stable_id = "a-new-lexically-first".into();
        new.digest = "c".repeat(64);
        new.delivery_digest = "d".repeat(64);
        reopened.append_offline_head(new).unwrap();
        let mixed = snapshot(&reopened.load().unwrap(), &recipient).unwrap();
        assert_eq!(mixed.heads.len(), 2);
        assert_eq!(mixed.receipts.len(), 2);
        assert_eq!(mixed.head_states.len(), 2);
        assert_eq!(mixed.head_states[0].stable_id, "a-new-lexically-first");
        assert_eq!(mixed.head_states[0].lifecycle, HeadLifecycle::Held);
        assert_eq!(mixed.head_states[1].stable_id, old.stable_id);
        assert_eq!(mixed.head_states[1].lifecycle, HeadLifecycle::Settled);
        assert!(mixed.claim.is_none());
    }

    #[test]
    fn old_single_stage_journals_reload_and_conflicting_resolution_replay_fails_closed() {
        let admitted_only = temporary_store();
        let old_head = head();
        admitted_only.append_offline_head(old_head.clone()).unwrap();
        let old_claim = admitted_only
            .claim_next(&old_head.recipient)
            .unwrap()
            .unwrap();
        admitted_only
            .resolve_claim(&old_claim.claim_id, ClaimResolutionOutcome::Admitted)
            .unwrap();
        let old_reload = MailboxStore::open(admitted_only.stream_path.parent().unwrap()).unwrap();
        assert_eq!(
            old_reload.claim_next(&old_head.recipient).unwrap(),
            Some(old_claim.clone())
        );
        let old_snapshot =
            crate::mailbox_v1::snapshot(&old_reload.load().unwrap(), &old_head.recipient).unwrap();
        assert_eq!(old_snapshot.claim, Some(old_claim));
        assert_eq!(
            old_snapshot.head_states[0].lifecycle,
            crate::mailbox_v1::HeadLifecycle::Admitted
        );

        let store = temporary_store();
        let high = head();
        store.append_offline_head(high.clone()).unwrap();
        let claim = store.claim_next(&high.recipient).unwrap().unwrap();
        store
            .resolve_claim(&claim.claim_id, ClaimResolutionOutcome::Settled)
            .unwrap();
        let reopened = MailboxStore::open(store.stream_path.parent().unwrap()).unwrap();
        assert_eq!(
            reopened.load().unwrap().resolutions[&claim.claim_id].outcome,
            ClaimResolutionOutcome::Settled
        );
        assert_eq!(reopened.claim_next(&high.recipient).unwrap(), None);
        let settled_snapshot =
            crate::mailbox_v1::snapshot(&reopened.load().unwrap(), &high.recipient).unwrap();
        assert_eq!(settled_snapshot.claim, None);
        assert_eq!(
            settled_snapshot.head_states[0].lifecycle,
            crate::mailbox_v1::HeadLifecycle::Settled
        );
        assert_eq!(settled_snapshot.heads.len(), 1);
        assert_eq!(settled_snapshot.receipts.len(), 1);
        store
            .append_synced(&MailboxRecord::Resolution {
                resolution: ClaimResolution {
                    claim_id: claim.claim_id.clone(),
                    outcome: ClaimResolutionOutcome::Admitted,
                },
            })
            .unwrap();
        assert_eq!(reopened.load(), Err(MailboxError::ConflictingDuplicate));
    }

    #[test]
    fn claimed_record_recovers_without_notification_or_reclaim() {
        let store = temporary_store();
        let head = head();
        store
            .append_head_and_receipt(head.clone(), receipt(&head))
            .unwrap();
        let claim = Claim {
            claim_id: "claim-1".into(),
            recipient: head.recipient.clone(),
            stable_id: head.stable_id.clone(),
            revision: 1,
            digest: head.digest.clone(),
        };
        store.claim(claim.clone()).unwrap();
        let reopened = MailboxStore::open(store.stream_path.parent().unwrap()).unwrap();
        assert_eq!(reopened.load().unwrap().claims[&head.stable_id], claim);
        assert_eq!(
            reopened.claim(Claim {
                claim_id: "claim-2".into(),
                ..claim.clone()
            }),
            Err(MailboxError::ClaimAlreadyExists)
        );
    }
}
