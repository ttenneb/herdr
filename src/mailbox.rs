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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipientKey {
    pub recipient_id: String,
    pub generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxHead {
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub delivery_digest: String,
    pub recipient: RecipientKey,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReceipt {
    pub delivery_digest: String,
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub status: ReceiptStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Admitted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub claim_id: String,
    pub recipient: RecipientKey,
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
}

/// Append-only stream. Records themselves are immutable; recovered state is a projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailboxRecord {
    Head { head: MailboxHead },
    Receipt { receipt: AdmissionReceipt },
    Claim { claim: Claim },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveredMailbox {
    pub heads: BTreeMap<String, MailboxHead>,
    pub receipts: BTreeMap<String, AdmissionReceipt>,
    pub claims: BTreeMap<String, Claim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxError {
    InvalidRecord,
    ConflictingDuplicate,
    MissingHead,
    ClaimAlreadyExists,
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
