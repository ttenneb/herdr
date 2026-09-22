use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailboxResolveParams {
    pub caller: String,
    pub grant_id: String,
    pub recipient: crate::mailbox::RecipientKey,
    #[serde(flatten)]
    pub resolve: crate::mailbox_v1::Resolve,
}
