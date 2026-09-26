use serde::{Deserialize, Serialize};

use super::{AgentSessionInfo, AgentStatus};

pub const HANDOFF_VERSION: u8 = 1;
pub const MAX_HANDOFF_BYTES: usize = 16 * 1024;
pub const MAX_SUMMARY_BYTES: usize = 2 * 1024;
pub const MAX_ARTIFACT_REFS: usize = 8;
pub const MAX_ARTIFACT_VALUE_BYTES: usize = 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_DIGEST_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalHerdrIdentity {
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub agent_session: AgentSessionInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffKind {
    Assignment,
    Blocker,
    Answer,
    Report,
    Cancel,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Commit,
    File,
    Diff,
    Receipt,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    pub kind: ArtifactKind,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HandoffTask {
    pub id: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HerdrHandoff {
    pub version: u8,
    pub message_id: String,
    pub created_at: String,
    pub sender: CanonicalHerdrIdentity,
    pub recipient: CanonicalHerdrIdentity,
    pub kind: HandoffKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<HandoffTask>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HandoffValidateParams {
    pub envelope: HerdrHandoff,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HandoffSendParams {
    pub envelope: HerdrHandoff,
    #[serde(default, flatten)]
    pub send: super::MessageSendOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffTransportOutcome {
    RuntimeTransactionAdmitted,
    /// Durably queued in the recipient's Messages; the recipient runs it when
    /// it next picks up work.
    MailboxAdmitted,
    SenderIdentityMismatch,
    RecipientIdentityMismatch,
    RecipientNotReady,
    RecipientBlocked,
    RecipientNotForeground,
    QueueFull,
    PayloadTooLarge,
    TransportClosed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HandoffTransportReceipt {
    pub version: u8,
    pub message_id: String,
    pub sender: CanonicalHerdrIdentity,
    pub recipient: CanonicalHerdrIdentity,
    pub attempted_path: String,
    pub target_status: AgentStatus,
    pub target_interactive_ready: bool,
    pub target_state_change_seq: u64,
    pub outcome: HandoffTransportOutcome,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<super::MessageDelivery>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffValidationError {
    pub field: &'static str,
    pub message: String,
}

impl std::fmt::Display for HandoffValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl HerdrHandoff {
    pub fn validate(&self) -> Result<(), HandoffValidationError> {
        if self.version != HANDOFF_VERSION {
            return invalid("version", format!("must be {HANDOFF_VERSION}"));
        }
        validate_text("messageId", &self.message_id, MAX_ID_BYTES, false)?;
        validate_text("createdAt", &self.created_at, MAX_ID_BYTES, false)?;
        validate_identity("sender", &self.sender)?;
        validate_identity("recipient", &self.recipient)?;
        if let Some(value) = &self.correlation_id {
            validate_text("correlationId", value, MAX_ID_BYTES, false)?;
        }
        if let Some(value) = &self.reply_to_id {
            validate_text("replyToId", value, MAX_ID_BYTES, false)?;
        }
        if let Some(task) = &self.task {
            match &task.id {
                serde_json::Value::String(value) => {
                    validate_text("task.id", value, MAX_ID_BYTES, false)?
                }
                serde_json::Value::Number(_) => {}
                _ => return invalid("task.id", "must be a string or number"),
            }
            if let Some(value) = &task.attempt_id {
                validate_text("task.attemptId", value, MAX_ID_BYTES, false)?;
            }
        }
        validate_text("summary", &self.summary, MAX_SUMMARY_BYTES, true)?;
        if self.artifact_refs.len() > MAX_ARTIFACT_REFS {
            return invalid(
                "artifactRefs",
                format!("must contain at most {MAX_ARTIFACT_REFS} entries"),
            );
        }
        for artifact in &self.artifact_refs {
            validate_text(
                "artifactRefs.value",
                &artifact.value,
                MAX_ARTIFACT_VALUE_BYTES,
                false,
            )?;
            if let Some(digest) = &artifact.digest {
                validate_text("artifactRefs.digest", digest, MAX_DIGEST_BYTES, false)?;
            }
        }
        let encoded = serde_json::to_vec(self).map_err(|err| HandoffValidationError {
            field: "envelope",
            message: err.to_string(),
        })?;
        if encoded.len() > MAX_HANDOFF_BYTES {
            return invalid(
                "envelope",
                format!("encoded envelope exceeds {MAX_HANDOFF_BYTES} bytes"),
            );
        }
        Ok(())
    }

    pub fn prompt_text(&self) -> String {
        format!(
            "HERDR HANDOFF v{}\n{}",
            self.version,
            serde_json::to_string(self).expect("validated handoff serializes")
        )
    }
}

fn validate_identity(
    prefix: &'static str,
    identity: &CanonicalHerdrIdentity,
) -> Result<(), HandoffValidationError> {
    validate_text(prefix, &identity.workspace_id, MAX_ID_BYTES, false)?;
    validate_text(prefix, &identity.pane_id, MAX_ID_BYTES, false)?;
    validate_text(prefix, &identity.terminal_id, MAX_ID_BYTES, false)?;
    validate_text(prefix, &identity.agent_session.source, MAX_ID_BYTES, false)?;
    validate_text(prefix, &identity.agent_session.agent, MAX_ID_BYTES, false)?;
    validate_text(prefix, &identity.agent_session.value, 1024, false)
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    allow_newline: bool,
) -> Result<(), HandoffValidationError> {
    if value.is_empty() {
        return invalid(field, "must not be empty");
    }
    if value.len() > max_bytes {
        return invalid(field, format!("exceeds {max_bytes} UTF-8 bytes"));
    }
    if value
        .chars()
        .any(|ch| is_forbidden_control(ch, allow_newline))
    {
        return invalid(
            field,
            "contains a terminal, ANSI, bidi, or unsafe control character",
        );
    }
    Ok(())
}

fn is_forbidden_control(ch: char, allow_newline: bool) -> bool {
    if ch == '\n' && allow_newline {
        return false;
    }
    ch.is_control()
        || matches!(ch, '\u{001b}' | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn invalid<T>(
    field: &'static str,
    message: impl Into<String>,
) -> Result<T, HandoffValidationError> {
    Err(HandoffValidationError {
        field,
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRefKind;

    fn identity(pane: &str) -> CanonicalHerdrIdentity {
        CanonicalHerdrIdentity {
            workspace_id: "w1".into(),
            pane_id: pane.into(),
            terminal_id: format!("t-{pane}"),
            agent_session: AgentSessionInfo {
                source: "hook".into(),
                agent: "pi".into(),
                kind: AgentSessionRefKind::Id,
                value: format!("s-{pane}"),
            },
        }
    }

    fn valid() -> HerdrHandoff {
        HerdrHandoff {
            version: 1,
            message_id: "m1".into(),
            created_at: "2026-09-18T00:00:00Z".into(),
            sender: identity("w1:p1"),
            recipient: identity("w1:p2"),
            kind: HandoffKind::Report,
            correlation_id: None,
            reply_to_id: None,
            task: None,
            summary: "done".into(),
            artifact_refs: vec![],
        }
    }

    #[test]
    fn accepts_bounded_envelope() {
        valid().validate().unwrap();
    }

    #[test]
    fn rejects_controls_and_bidi() {
        for summary in ["bad\u{1b}[31m", "bad\u{202e}txt", "bad\0txt"] {
            let mut value = valid();
            value.summary = summary.into();
            assert!(value.validate().is_err());
        }
    }

    #[test]
    fn rejects_bounds() {
        let mut value = valid();
        value.summary = "x".repeat(MAX_SUMMARY_BYTES + 1);
        assert!(value.validate().is_err());
        let mut value = valid();
        value.artifact_refs = (0..=MAX_ARTIFACT_REFS)
            .map(|_| ArtifactRef {
                kind: ArtifactKind::File,
                value: "x".into(),
                digest: None,
            })
            .collect();
        assert!(value.validate().is_err());
    }
}
