use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The kernel peer of a main-socket API connection, set by the server for
/// `mailbox.*` requests (clients cannot supply it). `None` on the params
/// means an in-process request (server-internal, tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiPeer {
    /// The peer's credentials could not be read.
    Unknown,
    /// The connecting process, identified by PID and kernel start time.
    Process { pid: u32, start_ticks: u64 },
}

/// Untrusted selectors for a server-owned offline mailbox route. The server
/// resolves both values against its authenticated local route; they are not
/// recipient or grant authority by themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxOfflineSubmitParams {
    pub caller: String,
    pub grant_id: String,
    pub recipient: crate::mailbox::RecipientKey,
    #[serde(flatten)]
    pub submit: crate::mailbox_v1::Submit,
    /// Server-set main-socket peer; never deserialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub api_peer: Option<ApiPeer>,
}

/// Consumer operation selectors. The server selects the durable head and mints
/// the claim only after matching its current active generation-bound capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxClaimParams {
    pub caller: String,
    pub grant_id: String,
    pub recipient: crate::mailbox::RecipientKey,
    #[serde(flatten)]
    pub claim: crate::mailbox_v1::ClaimRequest,
    /// Server-set main-socket peer; never deserialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub api_peer: Option<ApiPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxResolveParams {
    pub caller: String,
    pub grant_id: String,
    pub recipient: crate::mailbox::RecipientKey,
    #[serde(flatten)]
    pub resolve: crate::mailbox_v1::Resolve,
    /// Server-set main-socket peer; never deserialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub api_peer: Option<ApiPeer>,
}

/// Read selectors for the server-owned stable-recipient mailbox projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxSnapshotParams {
    pub caller: String,
    pub grant_id: String,
    pub recipient: crate::mailbox::RecipientKey,
    pub protocol: String,
    /// Server-set main-socket peer; never deserialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub api_peer: Option<ApiPeer>,
}

/// Untrusted edit intent. The caller/grant/recipient selectors are constrained
/// by the current server-issued mailbox scope; `edit` must CAS the exact head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxEditParams {
    pub caller: String,
    pub grant_id: String,
    pub recipient: crate::mailbox::RecipientKey,
    #[serde(flatten)]
    pub edit: crate::mailbox_v1::Edit,
    /// Server-set main-socket peer; never deserialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub api_peer: Option<ApiPeer>,
}
