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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    Head { head: MailboxHead },
    Receipt { receipt: AdmissionReceipt },
    Claim { claim: Claim },
    Resolution { resolution: ClaimResolution },
    Grant { grant: MailboxGrant },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveredMailbox {
    pub heads: BTreeMap<String, MailboxHead>,
    pub receipts: BTreeMap<String, AdmissionReceipt>,
    pub claims: BTreeMap<String, Claim>,
    pub resolutions: BTreeMap<String, ClaimResolution>,
    pub grants: BTreeMap<String, MailboxGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxError {
    InvalidRecord,
    ConflictingDuplicate,
    MissingHead,
    ClaimAlreadyExists,
    ClaimAlreadyResolved,
    MissingClaim,
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
        Ok(Self {
            stream_path: directory.as_ref().join(RECORD_STREAM_FILE),
            lock_path: directory.as_ref().join(LOCK_FILE),
        })
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
            if let Some(existing) = recovered.receipts.get(&receipt.delivery_digest) {
                return if existing == &receipt {
                    Ok(())
                } else {
                    Err(MailboxError::ConflictingDuplicate)
                };
            }
            if let Some(existing) = recovered.heads.get(&head.stable_id) {
                if existing != &head {
                    return Err(MailboxError::ConflictingDuplicate);
                }
            } else {
                self.append_synced(&MailboxRecord::Head { head })?;
            }
            // The preceding append_synced performed File::sync_all before this receipt is written.
            self.append_synced(&MailboxRecord::Receipt { receipt })
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
                        && !recovered.resolutions.contains_key(&claim.claim_id)
                })
                .cloned()
            {
                return Ok(Some(existing));
            }
            let Some(head) = recovered.heads.values().find(|head| {
                &head.recipient == recipient && !recovered.claims.contains_key(&head.stable_id)
            }) else {
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

    /// Persists a single resolution for the claimed work. Matching retries read
    /// back the first resolution; a conflicting replay cannot change outcome.
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
                return if existing == &resolution {
                    Ok(existing.clone())
                } else {
                    Err(MailboxError::ClaimAlreadyResolved)
                };
            }
            self.append_synced(&MailboxRecord::Resolution {
                resolution: resolution.clone(),
            })?;
            Ok(resolution)
        })
    }

    fn append_synced(&self, record: &MailboxRecord) -> Result<(), MailboxError> {
        let mut stream = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.stream_path)?;
        serde_json::to_writer(&mut stream, record)
            .map_err(|error| MailboxError::Io(error.to_string()))?;
        stream.write_all(b"\n")?;
        stream.sync_all()?;
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
        let result = operation();
        #[cfg(unix)]
        unsafe {
            use std::os::fd::AsRawFd;
            libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
        }
        result
    }
}

impl RecoveredMailbox {
    fn apply(&mut self, record: MailboxRecord) -> Result<(), MailboxError> {
        match record {
            MailboxRecord::Head { head } => {
                insert_exact(&mut self.heads, head.stable_id.clone(), head)
            }
            MailboxRecord::Receipt { receipt } => {
                insert_exact(&mut self.receipts, receipt.delivery_digest.clone(), receipt)
            }
            MailboxRecord::Claim { claim } => {
                insert_exact(&mut self.claims, claim.stable_id.clone(), claim)
            }
            MailboxRecord::Resolution { resolution } => insert_exact(
                &mut self.resolutions,
                resolution.claim_id.clone(),
                resolution,
            ),
            MailboxRecord::Grant { grant } => {
                insert_exact(&mut self.grants, grant.grant_id.clone(), grant)
            }
        }
    }
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
