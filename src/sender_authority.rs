//! Synchronous, server-owned write-ahead authority state for sender lifecycle transitions.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SenderAuthorityRecord {
    pub sender_key: String,
    pub process_generation: u64,
    pub phase: SenderAuthorityPhase,
    pub transition_revision: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SenderAuthorityPhase {
    Preparing,
    Issued,
    Active,
    Invalidated,
}
impl SenderAuthorityRecord {
    pub(crate) fn authoritative(&self) -> bool {
        self.phase == SenderAuthorityPhase::Active
    }
}
pub(crate) struct SenderAuthorityStore {
    path: PathBuf,
}
impl SenderAuthorityStore {
    pub(crate) fn open(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir.as_ref())?;
        Ok(Self {
            path: dir.as_ref().join("sender-authority.json"),
        })
    }
    pub(crate) fn load(&self) -> std::io::Result<Option<SenderAuthorityRecord>> {
        match std::fs::read(&self.path) {
            Ok(v) => Ok(Some(
                serde_json::from_slice(&v).map_err(std::io::Error::other)?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub(crate) fn cas(
        &self,
        expected_revision: Option<u64>,
        next: SenderAuthorityRecord,
    ) -> std::io::Result<()> {
        let current = self.load()?;
        if current.as_ref().map(|v| v.transition_revision) != expected_revision
            || next.process_generation == 0
            || next.transition_revision == 0
        {
            return Err(std::io::Error::other("sender authority CAS rejected"));
        }
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        use std::io::Write;
        f.write_all(&serde_json::to_vec(&next).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
    /// Promotes only the exact committed launch generation after a server-owned
    /// lifecycle observation. Preparing and Issued remain non-authoritative
    /// until this version-checked transition succeeds.
    pub(crate) fn promote_active(
        &self,
        sender_key: &str,
        process_generation: u64,
    ) -> std::io::Result<SenderAuthorityRecord> {
        let current = self
            .load()?
            .ok_or_else(|| std::io::Error::other("sender authority missing"))?;
        if current.sender_key != sender_key
            || current.process_generation != process_generation
            || !matches!(
                current.phase,
                SenderAuthorityPhase::Preparing | SenderAuthorityPhase::Issued
            )
        {
            return Err(std::io::Error::other("sender authority promotion rejected"));
        }
        let next_revision = current
            .transition_revision
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("sender authority revision exhausted"))?;
        let active = SenderAuthorityRecord {
            phase: SenderAuthorityPhase::Active,
            transition_revision: next_revision,
            ..current
        };
        self.cas(Some(next_revision - 1), active.clone())?;
        Ok(active)
    }

    /// Invalidates only the exact active sender generation. A later exit or
    /// replacement cannot revoke a newer record.
    pub(crate) fn invalidate_active(
        &self,
        sender_key: &str,
        process_generation: u64,
    ) -> std::io::Result<SenderAuthorityRecord> {
        let current = self
            .load()?
            .ok_or_else(|| std::io::Error::other("sender authority missing"))?;
        if current.sender_key != sender_key
            || current.process_generation != process_generation
            || current.phase != SenderAuthorityPhase::Active
        {
            return Err(std::io::Error::other(
                "sender authority invalidation rejected",
            ));
        }
        let next_revision = current
            .transition_revision
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("sender authority revision exhausted"))?;
        let invalidated = SenderAuthorityRecord {
            phase: SenderAuthorityPhase::Invalidated,
            transition_revision: next_revision,
            ..current
        };
        self.cas(Some(next_revision - 1), invalidated.clone())?;
        Ok(invalidated)
    }

    pub(crate) fn recover(&self) -> std::io::Result<Option<SenderAuthorityRecord>> {
        let Some(mut r) = self.load()? else {
            return Ok(None);
        };
        if !r.authoritative() {
            r.phase = SenderAuthorityPhase::Invalidated;
            r.transition_revision += 1;
            self.cas(Some(r.transition_revision - 1), r.clone())?;
        }
        Ok(Some(r))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_invalidates_unconfirmed_and_cas_rejects_stale() {
        let d = std::env::temp_dir().join(format!("herdr-sa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let s = SenderAuthorityStore::open(&d).unwrap();
        s.cas(
            None,
            SenderAuthorityRecord {
                sender_key: "s".into(),
                process_generation: 1,
                phase: SenderAuthorityPhase::Preparing,
                transition_revision: 1,
            },
        )
        .unwrap();
        assert!(!s.recover().unwrap().unwrap().authoritative());
        assert!(s
            .cas(
                Some(1),
                SenderAuthorityRecord {
                    sender_key: "s".into(),
                    process_generation: 1,
                    phase: SenderAuthorityPhase::Active,
                    transition_revision: 2
                }
            )
            .is_err());
        let _ = std::fs::remove_dir_all(d);
    }
}
