//! Default-off authority, authenticated-route, and wire primitives for direct messaging.
//!
//! This module opens no listener, injects no PTY input, and performs no file-based
//! recipient discovery. Opaque channels are produced only by crate-owned verification;
//! grants and tickets are then bound to a live, negotiated manifest route.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::delegation::{DelegationId, Delegations};

pub const FIXTURE_DIGEST: &str = "16d169e6c9b472b864c2c97e6d77c9268dc98d0d68c75154fa23ed87d2729129";
pub const PROTOCOL_VERSION: u16 = 1;
/// Version of the durable Gate/mailbox admission contract. This is deliberately
/// independent of any Pi background-submit or model-turn API.
pub const GATE_MAILBOX_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_SUBJECT_BYTES: usize = 160;
pub const MAX_BODY_BYTES: usize = 16_384;
pub const MAX_DESCRIPTION_BYTES: usize = 512;
pub const MAX_ADVISORY_GRANTS_PER_PRINCIPAL: usize = 16;
pub const MAX_ADVISORY_TTL_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Assignment,
    Report,
    Blocker,
    Question,
    Completion,
    Advisory,
    Correction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Send,
    Edit,
    Receipt,
    AgentStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    ControllerPm,
    PmTpm,
    TpmOwner,
    OwnerHelper,
    ChildParentReport,
    BoundedPeerAdvisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantClass {
    Standard,
    Advisory,
    StopOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionGeneration {
    pub session: String,
    pub generation: u64,
    pub delegation_id: DelegationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteScope {
    pub repository: String,
    pub worktree: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalBinding {
    pub identity: SessionGeneration,
    pub scope: RouteScope,
}

/// Immutable view built only from persisted delegation records and exact bindings.
#[derive(Debug, Clone)]
pub struct CanonicalTopology {
    revision: u64,
    parent: HashMap<DelegationId, Option<DelegationId>>,
    bindings: HashMap<DelegationId, PrincipalBinding>,
}

impl CanonicalTopology {
    pub fn from_persisted(
        delegations: &Delegations,
        bindings: impl IntoIterator<Item = PrincipalBinding>,
        revision: u64,
    ) -> Result<Self, TransportError> {
        if revision == 0 {
            return Err(TransportError::AuthorityUnconfirmed);
        }
        let parent: HashMap<_, _> = delegations
            .records()
            .values()
            .filter(|record| !record.tombstone && record.pane_id.is_some())
            .map(|record| (record.id, record.parent_id))
            .collect();
        let mut by_id = HashMap::new();
        for binding in bindings {
            let id = binding.identity.delegation_id;
            if !parent.contains_key(&id) || by_id.insert(id, binding).is_some() {
                return Err(TransportError::AuthorityUnconfirmed);
            }
        }
        if by_id.len() != parent.len() {
            return Err(TransportError::AuthorityUnconfirmed);
        }
        Ok(Self {
            revision,
            parent,
            bindings: by_id,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn exact_binding(&self, principal: &SessionGeneration, scope: &RouteScope) -> bool {
        self.bindings
            .get(&principal.delegation_id)
            .is_some_and(|binding| &binding.identity == principal && &binding.scope == scope)
    }

    fn direct_parent(&self, child: DelegationId) -> Option<DelegationId> {
        self.parent.get(&child).copied().flatten()
    }

    fn is_ancestor(&self, ancestor: DelegationId, node: DelegationId) -> bool {
        let mut seen = HashSet::new();
        let mut current = Some(node);
        while let Some(id) = current {
            if !seen.insert(id) {
                return false;
            }
            if id == ancestor {
                return true;
            }
            current = self.direct_parent(id);
        }
        false
    }

    fn valid_edge(&self, request: &GrantRequest) -> bool {
        let issuer = request.issuer.delegation_id;
        let recipient = request.recipient.delegation_id;
        match request.edge_kind {
            EdgeKind::ControllerPm
            | EdgeKind::PmTpm
            | EdgeKind::TpmOwner
            | EdgeKind::OwnerHelper => self.direct_parent(recipient) == Some(issuer),
            EdgeKind::ChildParentReport => self.direct_parent(issuer) == Some(recipient),
            EdgeKind::BoundedPeerAdvisory => {
                let Some(authorizer) = request.authorized_by.as_ref() else {
                    return false;
                };
                let common = authorizer.delegation_id;
                issuer != recipient
                    && common != issuer
                    && common != recipient
                    && self.exact_binding(authorizer, &request.scope)
                    && self.is_ancestor(common, issuer)
                    && self.is_ancestor(common, recipient)
                    && !self.is_ancestor(issuer, recipient)
                    && !self.is_ancestor(recipient, issuer)
            }
        }
    }
}

// Channel proof is intentionally private. Live adapters must verify a real descriptor/socket
// and then call these crate-only constructors; public callers cannot self-assert peer facts.
#[allow(dead_code)] // live OS adapters are deliberately not wired in this default-off slice
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChannelProof {
    AnonymousDescriptor {
        descriptor_generation: u64,
    },
    LocalPeer {
        uid: u32,
        pid: u32,
        foreground_pi_pid: u32,
        terminal_generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBinding {
    id: String,
    recipient: SessionGeneration,
    proof: ChannelProof,
}

impl ChannelBinding {
    /// Called only after a crate-owned adapter has installed `close-on-exec` on an
    /// anonymously inherited descriptor for this exact recipient generation.
    #[allow(dead_code)] // reserved for the future crate-owned descriptor adapter
    fn verified_anonymous(
        id: String,
        recipient: SessionGeneration,
        descriptor_generation: u64,
        close_on_exec_installed: bool,
    ) -> Result<Self, TransportError> {
        if !close_on_exec_installed
            || id.is_empty()
            || descriptor_generation != recipient.generation
        {
            return Err(TransportError::ChannelGenerationMismatch);
        }
        Ok(Self {
            id,
            recipient,
            proof: ChannelProof::AnonymousDescriptor {
                descriptor_generation,
            },
        })
    }

    /// Called only with OS-derived peer credentials and a crate-verified `/proc` ancestry result.
    #[allow(clippy::too_many_arguments, dead_code)] // reserved for the future crate-owned OS adapter
    fn verified_local_socket(
        id: String,
        recipient: SessionGeneration,
        platform_supported: bool,
        os_peer_uid: u32,
        process_uid: u32,
        os_peer_pid: u32,
        foreground_pi_pid: u32,
        peer_in_foreground_tree: bool,
        terminal_generation: u64,
    ) -> Result<Self, TransportError> {
        if !platform_supported {
            return Err(TransportError::ChannelUnsupported);
        }
        if id.is_empty()
            || os_peer_uid != process_uid
            || os_peer_pid == 0
            || foreground_pi_pid == 0
            || !peer_in_foreground_tree
        {
            return Err(TransportError::ChannelPeerMismatch);
        }
        if terminal_generation != recipient.generation {
            return Err(TransportError::ChannelGenerationMismatch);
        }
        Ok(Self {
            id,
            recipient,
            proof: ChannelProof::LocalPeer {
                uid: os_peer_uid,
                pid: os_peer_pid,
                foreground_pi_pid,
                terminal_generation,
            },
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn recipient(&self) -> &SessionGeneration {
        &self.recipient
    }

    pub fn select_preferred(
        anonymous: Option<Self>,
        local_socket: Option<Self>,
    ) -> Result<Self, TransportError> {
        if let Some(binding) = anonymous {
            if matches!(binding.proof, ChannelProof::AnonymousDescriptor { .. }) {
                return Ok(binding);
            }
            return Err(TransportError::ChannelPeerMismatch);
        }
        let binding = local_socket.ok_or(TransportError::ChannelUnsupported)?;
        if matches!(binding.proof, ChannelProof::LocalPeer { .. }) {
            Ok(binding)
        } else {
            Err(TransportError::ChannelPeerMismatch)
        }
    }
}

/// Server-owned facts for a mailbox channel. This type deliberately has no serde
/// implementation and cannot be constructed from an API request or Pi attachment.
///
/// The API boundary must obtain `recipient` and `foreground_pi_pid` from its live
/// terminal/agent registry, rather than from the client connection or mailbox wire data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustedMailboxChannelContext {
    binding: ChannelBinding,
    foreground_pi_pid: u32,
}

impl TrustedMailboxChannelContext {
    /// Verifies a Unix-domain peer against OS credentials and the server-owned foreground
    /// Pi process. `recipient`, `binding_id`, `foreground_pi_pid`, and `terminal_generation`
    /// are trusted lifecycle state, never values decoded from a mailbox request.
    #[cfg(target_os = "linux")]
    pub(crate) fn from_verified_local_socket(
        socket_fd: std::os::fd::RawFd,
        recipient: SessionGeneration,
        binding_id: String,
        foreground_pi_pid: u32,
        terminal_generation: u64,
    ) -> Result<Self, TransportError> {
        let mut credential = std::mem::MaybeUninit::<libc::ucred>::zeroed();
        let mut credential_len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the caller owns a live Unix-domain socket descriptor; getsockopt writes
        // exactly the supplied ucred buffer or fails without exposing uninitialized data.
        let result = unsafe {
            libc::getsockopt(
                socket_fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credential.as_mut_ptr().cast(),
                &mut credential_len,
            )
        };
        if result != 0 || credential_len as usize != std::mem::size_of::<libc::ucred>() {
            return Err(TransportError::ChannelPeerMismatch);
        }
        // SAFETY: the successful getsockopt call initialized the complete ucred value.
        let credential = unsafe { credential.assume_init() };
        let peer_pid =
            u32::try_from(credential.pid).map_err(|_| TransportError::ChannelPeerMismatch)?;
        let peer_uid = credential.uid;
        let process_uid = unsafe { libc::geteuid() };
        let peer_in_foreground_tree =
            peer_pid == foreground_pi_pid || process_descends_from(peer_pid, foreground_pi_pid);
        let binding = ChannelBinding::verified_local_socket(
            binding_id,
            recipient,
            true,
            peer_uid,
            process_uid,
            peer_pid,
            foreground_pi_pid,
            peer_in_foreground_tree,
            terminal_generation,
        )?;
        Ok(Self {
            binding,
            foreground_pi_pid,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn from_verified_local_socket(
        _socket_fd: i32,
        _recipient: SessionGeneration,
        _binding_id: String,
        _foreground_pi_pid: u32,
        _terminal_generation: u64,
    ) -> Result<Self, TransportError> {
        Err(TransportError::ChannelUnsupported)
    }

    pub(crate) fn binding(&self) -> &ChannelBinding {
        &self.binding
    }

    pub(crate) fn foreground_pi_pid(&self) -> u32 {
        self.foreground_pi_pid
    }

    /// Produces a route only from this verified binding and the live manifest registry.
    pub(crate) fn negotiate(
        &self,
        manifests: &ManifestRegistry,
        now: u64,
    ) -> Result<NegotiatedRoute, TransportError> {
        manifests.negotiate(
            &self.binding,
            PROTOCOL_VERSION,
            FIXTURE_DIGEST,
            GATE_MAILBOX_VERSION,
            now,
        )
    }
}

#[cfg(target_os = "linux")]
fn process_descends_from(mut child: u32, ancestor: u32) -> bool {
    let mut seen = HashSet::new();
    while child != 0 && seen.insert(child) {
        if child == ancestor {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
            return false;
        };
        let Some(rest) = stat.get(stat.rfind(')').unwrap_or(0).saturating_add(2)..) else {
            return false;
        };
        let Some(parent) = rest
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
        else {
            return false;
        };
        child = parent;
    }
    false
}

/// A registered recipient channel, built exclusively from a server-owned terminal/agent
/// lifecycle event after `TrustedMailboxChannelContext` has verified the dedicated socket.
/// It intentionally contains no API wire, Pi attachment, or control-socket caller fields.
#[derive(Debug)]
struct RegisteredRecipientChannel {
    context: TrustedMailboxChannelContext,
    manifests: ManifestRegistry,
    topology: CanonicalTopology,
}

/// Server-owned registration table for live mailbox recipients. The API control socket must
/// never call this directly: registration needs a dedicated recipient channel plus terminal
/// lifecycle state. Removing a registration is the mandatory disconnect/reload/generation
/// invalidation step and makes all later discovery fail with `ManifestMissing`.
#[derive(Debug, Default)]
pub(crate) struct RecipientChannelRegistry {
    recipients: HashMap<SessionGeneration, RegisteredRecipientChannel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredRecipientInfo {
    pub(crate) recipient: SessionGeneration,
    pub(crate) binding_id: String,
    pub(crate) terminal_generation: u64,
    pub(crate) foreground_pi_pid: u32,
    pub(crate) topology_revision: u64,
}

impl RecipientChannelRegistry {
    /// Registers one recipient generation. `context` must have been constructed from the
    /// actual recipient Unix socket; `manifest` and `topology` are server lifecycle snapshots.
    /// A duplicate generation fails closed rather than silently replacing its authority.
    pub(crate) fn register(
        &mut self,
        context: TrustedMailboxChannelContext,
        manifest: LiveManifest,
        topology: CanonicalTopology,
        now: u64,
    ) -> Result<(), TransportError> {
        let recipient = context.binding().recipient().clone();
        if self.recipients.contains_key(&recipient) {
            return Err(TransportError::SessionReplaced);
        }
        let mut manifests = ManifestRegistry::default();
        manifests.register_atomic(manifest, context.binding(), now)?;
        self.recipients.insert(
            recipient,
            RegisteredRecipientChannel {
                context,
                manifests,
                topology,
            },
        );
        Ok(())
    }

    /// Returns only server-recorded facts for diagnostics and endpoint selection.
    pub(crate) fn registered(
        &self,
        recipient: &SessionGeneration,
    ) -> Option<RegisteredRecipientInfo> {
        let entry = self.recipients.get(recipient)?;
        Some(RegisteredRecipientInfo {
            recipient: entry.context.binding().recipient().clone(),
            binding_id: entry.context.binding().id().to_string(),
            terminal_generation: entry.context.binding().recipient().generation,
            foreground_pi_pid: entry.context.foreground_pi_pid(),
            topology_revision: entry.topology.revision(),
        })
    }

    /// Derives a route from the server-owned registration. The caller supplies a recipient
    /// selected from server state, not a request recipient or Pi attachment identity.
    pub(crate) fn with_authenticated_route<T>(
        &self,
        recipient: &SessionGeneration,
        now: u64,
        operation: impl FnOnce(
            &TrustedMailboxChannelContext,
            &ManifestRegistry,
            &CanonicalTopology,
            NegotiatedRoute,
        ) -> Result<T, TransportError>,
    ) -> Result<T, TransportError> {
        let entry = self
            .recipients
            .get(recipient)
            .ok_or(TransportError::ManifestMissing)?;
        let route = entry.context.negotiate(&entry.manifests, now)?;
        operation(&entry.context, &entry.manifests, &entry.topology, route)
    }

    /// Call from the terminal/agent lifecycle on socket close, reload, or generation change.
    /// The exact registered binding must match; a stale lifecycle event cannot remove a newer
    /// recipient registration.
    pub(crate) fn invalidate(
        &mut self,
        recipient: &SessionGeneration,
        binding_id: &str,
    ) -> Result<(), TransportError> {
        let entry = self
            .recipients
            .get(recipient)
            .ok_or(TransportError::ManifestMissing)?;
        if entry.context.binding().id() != binding_id {
            return Err(TransportError::ChannelPeerMismatch);
        }
        self.recipients.remove(recipient);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteIdentity {
    recipient: SessionGeneration,
    channel_binding: String,
    manifest_epoch: u64,
}

impl RouteIdentity {
    pub fn recipient(&self) -> &SessionGeneration {
        &self.recipient
    }
    pub fn channel_binding(&self) -> &str {
        &self.channel_binding
    }
    pub fn manifest_epoch(&self) -> u64 {
        self.manifest_epoch
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigProvenance {
    LinkedLocalOverride { path: PathBuf },
    PrimaryExplicitInheritance { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveManifest {
    pub recipient: SessionGeneration,
    pub protocol_min: u16,
    pub protocol_max: u16,
    pub fixture_digest: String,
    pub features: HashSet<String>,
    pub gate_mailbox_version: u16,
    pub package_revision: String,
    pub effective_config_digest: String,
    pub config_provenance: ConfigProvenance,
    pub registration_epoch: u64,
    pub expires_at: u64,
    pub channel_binding: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedRoute {
    identity: RouteIdentity,
    registry_generation: u64,
}

impl NegotiatedRoute {
    pub fn identity(&self) -> &RouteIdentity {
        &self.identity
    }
}

#[derive(Debug, Default)]
pub struct ManifestRegistry {
    current: Option<LiveManifest>,
    generation: u64,
    /// Never cleared by disconnect or reload; prevents stale manifest replay.
    registration_epoch_high_water: u64,
}

impl ManifestRegistry {
    pub fn register_atomic(
        &mut self,
        manifest: LiveManifest,
        channel: &ChannelBinding,
        now: u64,
    ) -> Result<(), TransportError> {
        validate_manifest(&manifest, channel, now)?;
        if manifest.registration_epoch <= self.registration_epoch_high_water {
            return Err(TransportError::ManifestReplaced);
        }
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or(TransportError::RegistryGenerationExhausted)?;
        self.generation = next_generation;
        self.registration_epoch_high_water = manifest.registration_epoch;
        self.current = Some(manifest);
        Ok(())
    }

    pub fn negotiate(
        &self,
        channel: &ChannelBinding,
        protocol: u16,
        fixture_digest: &str,
        gate_mailbox_version: u16,
        now: u64,
    ) -> Result<NegotiatedRoute, TransportError> {
        let manifest = self
            .current
            .as_ref()
            .ok_or(TransportError::ManifestMissing)?;
        validate_manifest(manifest, channel, now)?;
        if !(manifest.protocol_min..=manifest.protocol_max).contains(&protocol)
            || manifest.gate_mailbox_version != gate_mailbox_version
        {
            return Err(TransportError::ProtocolIncompatible);
        }
        if manifest.fixture_digest != fixture_digest {
            return Err(TransportError::FixtureDigestMismatch);
        }
        Ok(NegotiatedRoute {
            identity: RouteIdentity {
                recipient: manifest.recipient.clone(),
                channel_binding: manifest.channel_binding.clone(),
                manifest_epoch: manifest.registration_epoch,
            },
            registry_generation: self.generation,
        })
    }

    fn validate_route(&self, route: &NegotiatedRoute, now: u64) -> Result<(), TransportError> {
        let current = self
            .current
            .as_ref()
            .ok_or(TransportError::ManifestMissing)?;
        if route.registry_generation != self.generation
            || route.identity.recipient != current.recipient
            || route.identity.channel_binding != current.channel_binding
            || route.identity.manifest_epoch != current.registration_epoch
        {
            return Err(TransportError::ManifestReplaced);
        }
        if current.expires_at <= now {
            return Err(TransportError::ManifestStale);
        }
        Ok(())
    }

    pub fn validate_ticket(&self, ticket: &GrantTicket, now: u64) -> Result<(), TransportError> {
        self.validate_route(&ticket.route, now)
    }

    pub fn disconnect(&mut self, channel_binding: &str) -> Result<(), TransportError> {
        if self
            .current
            .as_ref()
            .is_some_and(|manifest| manifest.channel_binding == channel_binding)
        {
            self.invalidate()?;
        }
        Ok(())
    }

    pub fn reload(&mut self) -> Result<(), TransportError> {
        self.invalidate()
    }

    fn invalidate(&mut self) -> Result<(), TransportError> {
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or(TransportError::RegistryGenerationExhausted)?;
        self.generation = next_generation;
        self.current = None;
        Ok(())
    }
}

fn validate_manifest(
    manifest: &LiveManifest,
    channel: &ChannelBinding,
    now: u64,
) -> Result<(), TransportError> {
    if manifest.channel_binding != channel.id {
        return Err(TransportError::ManifestReplaced);
    }
    if manifest.recipient != channel.recipient {
        return Err(TransportError::ChannelGenerationMismatch);
    }
    if manifest.registration_epoch == 0
        || manifest.registration_epoch > now
        || manifest.expires_at <= now
    {
        return Err(TransportError::ManifestStale);
    }
    if manifest.fixture_digest != FIXTURE_DIGEST {
        return Err(TransportError::FixtureDigestMismatch);
    }
    if manifest.protocol_min == 0
        || manifest.protocol_min > manifest.protocol_max
        || !(manifest.protocol_min..=manifest.protocol_max).contains(&PROTOCOL_VERSION)
        || manifest.gate_mailbox_version != GATE_MAILBOX_VERSION
        || manifest.package_revision.is_empty()
        || !is_lower_hex(&manifest.effective_config_digest, 64)
    {
        return Err(TransportError::ProtocolIncompatible);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct GrantRequest {
    pub grant_id: String,
    pub task_id: String,
    pub delegation_id: DelegationId,
    pub edge_kind: EdgeKind,
    pub class: GrantClass,
    pub issuer: SessionGeneration,
    pub recipient: SessionGeneration,
    pub authorized_by: Option<SessionGeneration>,
    pub scope: RouteScope,
    pub message_kinds: HashSet<MessageKind>,
    pub effects: HashSet<Effect>,
    pub issued_at: u64,
    pub expires_at: u64,
    pub topology_revision: u64,
    pub grant_revision: u64,
    pub route: NegotiatedRoute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    Active,
    Revoking,
    Revoked,
    Expired,
}

#[derive(Debug, Clone)]
struct Grant {
    request: GrantRequest,
    status: GrantStatus,
    revoking_revision: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AuthorityContext {
    pub issuer: SessionGeneration,
    pub recipient: SessionGeneration,
    pub scope: RouteScope,
    pub topology_revision: u64,
    pub grant_revision: u64,
    pub connected: bool,
    pub authority_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantTicket {
    pub grant_id: String,
    pub grant_revision: u64,
    pub recipient: SessionGeneration,
    pub effect: Effect,
    pub kind: Option<MessageKind>,
    route: NegotiatedRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationFence {
    pub grant_id: String,
    pub revoked_revision: u64,
    pub recipient: SessionGeneration,
}

pub trait GateFenceInstaller {
    fn install_revocation_fence(
        &mut self,
        fence: &RevocationFence,
    ) -> Result<FenceReceipt, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FenceReceipt {
    pub grant_id: String,
    pub revoked_revision: u64,
    pub recipient: SessionGeneration,
    pub installed: bool,
}

#[derive(Debug, Default)]
pub struct GrantAuthority {
    grants: HashMap<String, Grant>,
    grant_revision: u64,
    topology_revision: u64,
    connected: bool,
    authority_confirmed: bool,
}

impl GrantAuthority {
    pub fn new(topology_revision: u64) -> Self {
        Self {
            topology_revision,
            connected: true,
            authority_confirmed: topology_revision != 0,
            ..Self::default()
        }
    }

    pub fn issue(
        &mut self,
        request: GrantRequest,
        topology: &CanonicalTopology,
        manifests: &ManifestRegistry,
        now: u64,
    ) -> Result<(), TransportError> {
        self.require_available()?;
        manifests.validate_route(&request.route, now)?;
        if request.route.identity.recipient != request.recipient {
            return Err(TransportError::GrantScopeMismatch);
        }
        if request.grant_id.is_empty() || self.grants.contains_key(&request.grant_id) {
            return Err(TransportError::GrantCollision);
        }
        if request.task_id.is_empty() {
            return Err(TransportError::GrantScopeMismatch);
        }
        self.require_next_revision(request.grant_revision)?;
        if topology.revision() != self.topology_revision
            || request.topology_revision != self.topology_revision
        {
            return Err(TransportError::TopologyStale);
        }
        if request.delegation_id != request.recipient.delegation_id
            || !topology.exact_binding(&request.issuer, &request.scope)
            || !topology.exact_binding(&request.recipient, &request.scope)
        {
            return Err(TransportError::GrantScopeMismatch);
        }
        if !topology.valid_edge(&request) {
            let issuer = request.issuer.delegation_id;
            let recipient = request.recipient.delegation_id;
            return Err(
                if topology.is_ancestor(issuer, recipient)
                    || topology.is_ancestor(recipient, issuer)
                {
                    TransportError::SkipLevelForbidden
                } else {
                    TransportError::LateralForbidden
                },
            );
        }
        if request.issued_at > now || request.expires_at <= now {
            return Err(TransportError::GrantExpired);
        }
        self.validate_class(&request, now)?;
        self.grant_revision = request.grant_revision;
        self.grants.insert(
            request.grant_id.clone(),
            Grant {
                request,
                status: GrantStatus::Active,
                revoking_revision: None,
            },
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ticket(
        &mut self,
        grant_id: &str,
        context: &AuthorityContext,
        route: &NegotiatedRoute,
        manifests: &ManifestRegistry,
        effect: Effect,
        kind: Option<MessageKind>,
        now: u64,
    ) -> Result<GrantTicket, TransportError> {
        self.require_available()?;
        manifests.validate_route(route, now)?;
        if !context.connected {
            return Err(TransportError::MirrorDisconnected);
        }
        if !context.authority_confirmed {
            return Err(TransportError::AuthorityUnconfirmed);
        }
        if context.topology_revision != self.topology_revision {
            return Err(TransportError::TopologyStale);
        }
        if context.grant_revision < self.grant_revision {
            return Err(TransportError::GrantRevisionRollback);
        }
        if context.grant_revision > self.grant_revision {
            return Err(TransportError::GrantRevisionGap);
        }
        let grant = self
            .grants
            .get_mut(grant_id)
            .ok_or(TransportError::GrantMissing)?;
        if grant.request.expires_at <= now {
            grant.status = GrantStatus::Expired;
            return Err(TransportError::GrantExpired);
        }
        match grant.status {
            GrantStatus::Active => {}
            GrantStatus::Revoking | GrantStatus::Revoked => {
                return Err(TransportError::GrantRevoked)
            }
            GrantStatus::Expired => return Err(TransportError::GrantExpired),
        }
        if context.issuer != grant.request.issuer || context.recipient != grant.request.recipient {
            return Err(TransportError::SessionReplaced);
        }
        if context.scope != grant.request.scope
            || route.identity != grant.request.route.identity
            || route.registry_generation != grant.request.route.registry_generation
        {
            return Err(TransportError::GrantScopeMismatch);
        }
        if !grant.request.effects.contains(&effect) {
            return Err(TransportError::OperationNotAllowed);
        }
        if let Some(kind) = kind {
            if !grant.request.message_kinds.contains(&kind) {
                return Err(if grant.request.class == GrantClass::Advisory {
                    TransportError::AdvisoryOperationForbidden
                } else {
                    TransportError::OperationNotAllowed
                });
            }
        }
        if grant.request.class == GrantClass::StopOnly && effect != Effect::AgentStop {
            return Err(TransportError::StopGrantRequired);
        }
        Ok(GrantTicket {
            grant_id: grant_id.into(),
            grant_revision: grant.request.grant_revision,
            recipient: grant.request.recipient.clone(),
            effect,
            kind,
            route: route.clone(),
        })
    }

    pub fn begin_revocation(
        &mut self,
        grant_id: &str,
        next_revision: u64,
    ) -> Result<RevocationFence, TransportError> {
        self.require_available()?;
        self.require_next_revision(next_revision)?;
        let grant = self
            .grants
            .get_mut(grant_id)
            .ok_or(TransportError::GrantMissing)?;
        if grant.status != GrantStatus::Active {
            return Err(TransportError::GrantRevoked);
        }
        grant.status = GrantStatus::Revoking;
        grant.revoking_revision = Some(next_revision);
        self.grant_revision = next_revision;
        Ok(RevocationFence {
            grant_id: grant_id.into(),
            revoked_revision: next_revision,
            recipient: grant.request.recipient.clone(),
        })
    }

    pub fn complete_revocation(&mut self, receipt: FenceReceipt) -> Result<(), TransportError> {
        let grant = self
            .grants
            .get_mut(&receipt.grant_id)
            .ok_or(TransportError::GrantMissing)?;
        if grant.status != GrantStatus::Revoking
            || !receipt.installed
            || grant.revoking_revision != Some(receipt.revoked_revision)
            || receipt.recipient != grant.request.recipient
        {
            return Err(TransportError::FenceUnconfirmed);
        }
        grant.status = GrantStatus::Revoked;
        Ok(())
    }

    pub fn revoke_with_gate(
        &mut self,
        grant_id: &str,
        next_revision: u64,
        gate: &mut impl GateFenceInstaller,
    ) -> Result<(), TransportError> {
        let fence = self.begin_revocation(grant_id, next_revision)?;
        let receipt = gate.install_revocation_fence(&fence)?;
        self.complete_revocation(receipt)
    }

    pub fn status(&self, grant_id: &str) -> Option<GrantStatus> {
        self.grants.get(grant_id).map(|grant| grant.status)
    }
    pub fn disconnect(&mut self) {
        self.connected = false;
        self.authority_confirmed = false;
    }

    pub fn replace_topology(
        &mut self,
        revision: u64,
        confirmed: bool,
    ) -> Result<(), TransportError> {
        if revision <= self.topology_revision {
            return Err(TransportError::GrantRevisionRollback);
        }
        self.topology_revision = revision;
        self.authority_confirmed = confirmed && revision != 0;
        self.connected = true;
        for grant in self.grants.values_mut() {
            grant.status = GrantStatus::Revoked;
        }
        Ok(())
    }

    fn require_available(&self) -> Result<(), TransportError> {
        if !self.connected {
            Err(TransportError::MirrorDisconnected)
        } else if !self.authority_confirmed {
            Err(TransportError::AuthorityUnconfirmed)
        } else {
            Ok(())
        }
    }

    fn require_next_revision(&self, revision: u64) -> Result<(), TransportError> {
        match revision.cmp(&self.grant_revision.saturating_add(1)) {
            std::cmp::Ordering::Less => Err(TransportError::GrantRevisionRollback),
            std::cmp::Ordering::Greater => Err(TransportError::GrantRevisionGap),
            std::cmp::Ordering::Equal => Ok(()),
        }
    }

    fn validate_class(&self, request: &GrantRequest, now: u64) -> Result<(), TransportError> {
        match request.class {
            GrantClass::Standard => {
                if request.edge_kind == EdgeKind::BoundedPeerAdvisory
                    || request.effects.contains(&Effect::AgentStop)
                {
                    return Err(TransportError::OperationNotAllowed);
                }
            }
            GrantClass::Advisory => {
                if request.edge_kind != EdgeKind::BoundedPeerAdvisory
                    || request.message_kinds != HashSet::from([MessageKind::Advisory])
                    || request.effects != HashSet::from([Effect::Send])
                    || request.expires_at.saturating_sub(request.issued_at) > MAX_ADVISORY_TTL_SECS
                {
                    return Err(TransportError::AdvisoryOperationForbidden);
                }
                let count = self
                    .grants
                    .values()
                    .filter(|grant| {
                        grant.status == GrantStatus::Active
                            && grant.request.expires_at > now
                            && grant.request.class == GrantClass::Advisory
                            && grant.request.issuer == request.issuer
                    })
                    .count();
                if count >= MAX_ADVISORY_GRANTS_PER_PRINCIPAL {
                    return Err(TransportError::AdvisoryLimit);
                }
            }
            GrantClass::StopOnly => {
                if request.edge_kind == EdgeKind::BoundedPeerAdvisory
                    || !request.message_kinds.is_empty()
                    || request.effects != HashSet::from([Effect::AgentStop])
                {
                    return Err(TransportError::StopGrantRequired);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ImminentUnauthorizedEffect,
    ImminentUnsafeEffect,
    HumanRequestedStop,
}

impl StopReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::ImminentUnauthorizedEffect => "imminent_unauthorized_effect",
            Self::ImminentUnsafeEffect => "imminent_unsafe_effect",
            Self::HumanRequestedStop => "human_requested_stop",
        }
    }
}

pub fn authorize_stop(
    ticket: &GrantTicket,
    target: &SessionGeneration,
    _reason: StopReason,
) -> Result<(), TransportError> {
    if ticket.effect != Effect::AgentStop || &ticket.recipient != target {
        return Err(TransportError::StopGrantRequired);
    }
    Ok(())
}

/// Strict direct-message wire operations. Unknown fields are rejected.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum WireOperation {
    Send(SendFrame),
    Edit(EditFrame),
    Receipt(ReceiptFrame),
    AgentStop(StopFrame),
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendFrame {
    pub grant_id: String,
    pub message_id: String,
    pub kind: MessageKind,
    pub priority: Priority,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub description: Option<String>,
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditFrame {
    pub message_id: String,
    pub expected_revision: u64,
    pub next_revision: u64,
    pub subject: String,
    pub body: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptFrame {
    pub message_id: String,
    pub correlation_id: String,
    pub payload_digest: String,
    pub recipient: SessionGeneration,
    pub outcome: AdmissionOutcome,
}

/// A transport write is not delivery. Only one of these durable Gate outcomes
/// resolves the sender's pending state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AdmissionOutcome {
    Admitted { admission_revision: u64 },
    Rejected { code: String },
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StopFrame {
    pub stop_grant_id: String,
    pub reason_code: StopReason,
    pub target_generation: u64,
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
}

/// Persist this snapshot before attempting transport. Pending entries survive
/// transport acceptance, disconnect, reload, and process recovery until an exact
/// Gate admission/rejection receipt reconciles them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryRecord {
    pub message_id: String,
    pub correlation_id: String,
    pub payload_digest: String,
    pub recipient: SessionGeneration,
    pub created_at: u64,
    pub transport_accepted_at: Option<u64>,
    pub resolution: Option<AdmissionOutcome>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderDeliverySnapshot {
    pub records: Vec<DeliveryRecord>,
}

#[derive(Debug, Default)]
pub struct SenderDeliveryJournal {
    by_correlation: HashMap<String, DeliveryRecord>,
}

impl SenderDeliveryJournal {
    pub fn restore(snapshot: SenderDeliverySnapshot) -> Result<Self, TransportError> {
        let mut journal = Self::default();
        for record in snapshot.records {
            journal.insert_new(record)?;
        }
        Ok(journal)
    }

    pub fn snapshot(&self) -> SenderDeliverySnapshot {
        let mut records: Vec<_> = self.by_correlation.values().cloned().collect();
        records.sort_by(|a, b| a.correlation_id.cmp(&b.correlation_id));
        SenderDeliverySnapshot { records }
    }

    /// Must be durably persisted before the corresponding send frame is written.
    pub fn begin(&mut self, record: DeliveryRecord) -> Result<(), TransportError> {
        if record.transport_accepted_at.is_some() || record.resolution.is_some() {
            return Err(TransportError::CorrelationConflict);
        }
        self.insert_new(record)
    }

    fn insert_new(&mut self, record: DeliveryRecord) -> Result<(), TransportError> {
        validate_delivery_record(&record)?;
        if let Some(outcome) = &record.resolution {
            validate_admission_outcome(outcome)?;
        }
        if let Some(existing) = self.by_correlation.get(&record.correlation_id) {
            return if existing == &record {
                Ok(())
            } else {
                Err(TransportError::CorrelationConflict)
            };
        }
        self.by_correlation
            .insert(record.correlation_id.clone(), record);
        Ok(())
    }

    /// Records only socket/transport acceptance. It intentionally leaves the
    /// delivery unresolved and therefore visible to reconciliation and recovery.
    pub fn record_transport_acceptance(
        &mut self,
        correlation_id: &str,
        accepted_at: u64,
    ) -> Result<(), TransportError> {
        let record = self
            .by_correlation
            .get_mut(correlation_id)
            .ok_or(TransportError::ReceiptMismatch)?;
        if record.resolution.is_some() {
            return Err(TransportError::CorrelationConflict);
        }
        match record.transport_accepted_at {
            Some(existing) if existing != accepted_at => Err(TransportError::CorrelationConflict),
            Some(_) => Ok(()),
            None => {
                record.transport_accepted_at = Some(accepted_at);
                Ok(())
            }
        }
    }

    /// Creates a duplicate-safe retry attempt with a fresh correlation. The
    /// immutable message id and payload digest let the Gate return the original
    /// admission without enqueueing the body twice.
    pub fn begin_retry(
        &mut self,
        prior_correlation_id: &str,
        fresh_correlation_id: String,
        created_at: u64,
    ) -> Result<(), TransportError> {
        let prior = self
            .by_correlation
            .get(prior_correlation_id)
            .ok_or(TransportError::ReceiptMismatch)?;
        if matches!(prior.resolution, Some(AdmissionOutcome::Admitted { .. })) {
            return Err(TransportError::DeliveryAlreadyAdmitted);
        }
        let retry = DeliveryRecord {
            message_id: prior.message_id.clone(),
            correlation_id: fresh_correlation_id,
            payload_digest: prior.payload_digest.clone(),
            recipient: prior.recipient.clone(),
            created_at,
            transport_accepted_at: None,
            resolution: None,
        };
        self.begin(retry)
    }

    pub fn reconcile(&mut self, receipt: &ReceiptFrame) -> Result<(), TransportError> {
        validate_receipt(receipt)?;
        let record = self
            .by_correlation
            .get(&receipt.correlation_id)
            .ok_or(TransportError::ReceiptMismatch)?;
        if record.message_id != receipt.message_id
            || record.payload_digest != receipt.payload_digest
            || record.recipient != receipt.recipient
        {
            return Err(TransportError::ReceiptMismatch);
        }
        if let Some(existing) = &record.resolution {
            return if existing == &receipt.outcome {
                Ok(())
            } else {
                Err(TransportError::CorrelationConflict)
            };
        }

        // An admitted duplicate-safe retry proves delivery of the immutable
        // message, so it reconciles all still-pending attempts for that exact
        // recipient and payload. A rejection resolves only its own attempt.
        if matches!(receipt.outcome, AdmissionOutcome::Admitted { .. }) {
            for candidate in self.by_correlation.values_mut().filter(|candidate| {
                candidate.message_id == receipt.message_id
                    && candidate.payload_digest == receipt.payload_digest
                    && candidate.recipient == receipt.recipient
            }) {
                if candidate.resolution.is_none() {
                    candidate.resolution = Some(receipt.outcome.clone());
                }
            }
        } else {
            self.by_correlation
                .get_mut(&receipt.correlation_id)
                .expect("receipt correlation was validated")
                .resolution = Some(receipt.outcome.clone());
        }
        Ok(())
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &DeliveryRecord> {
        self.by_correlation
            .values()
            .filter(|record| record.resolution.is_none())
    }

    /// Retention never removes unresolved sender uncertainty.
    pub fn retain_from(&mut self, cutoff: u64) {
        self.by_correlation
            .retain(|_, record| record.resolution.is_none() || record.created_at >= cutoff);
    }
}

fn validate_delivery_record(record: &DeliveryRecord) -> Result<(), TransportError> {
    if !is_lower_hex(&record.message_id, 32)
        || !is_lower_hex(&record.correlation_id, 32)
        || !is_lower_hex(&record.payload_digest, 64)
        || record.recipient.session.is_empty()
        || record.recipient.generation == 0
        || record.created_at == 0
    {
        return Err(TransportError::InvalidSchema);
    }
    Ok(())
}

fn validate_receipt(receipt: &ReceiptFrame) -> Result<(), TransportError> {
    if !is_lower_hex(&receipt.message_id, 32)
        || !is_lower_hex(&receipt.correlation_id, 32)
        || !is_lower_hex(&receipt.payload_digest, 64)
        || receipt.recipient.session.is_empty()
        || receipt.recipient.generation == 0
    {
        return Err(TransportError::InvalidSchema);
    }
    validate_admission_outcome(&receipt.outcome)
}

fn validate_admission_outcome(outcome: &AdmissionOutcome) -> Result<(), TransportError> {
    match outcome {
        AdmissionOutcome::Admitted { admission_revision } if *admission_revision > 0 => Ok(()),
        AdmissionOutcome::Rejected { code }
            if !code.is_empty()
                && code.len() <= 64
                && code.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                }) =>
        {
            Ok(())
        }
        _ => Err(TransportError::InvalidSchema),
    }
}

#[derive(Debug)]
enum CanonicalValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<CanonicalValue>),
    Object(BTreeMap<String, CanonicalValue>),
}

impl<'de> Deserialize<'de> for CanonicalValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = CanonicalValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("canonical JSON")
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(CanonicalValue::Null)
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(CanonicalValue::Null)
            }
            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(CanonicalValue::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(CanonicalValue::Number(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(CanonicalValue::Number(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(CanonicalValue::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if !is_nfc(v) {
                    return Err(E::custom("non-NFC string"));
                }
                Ok(CanonicalValue::String(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                self.visit_str(&v)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(CanonicalValue::Array(values))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !is_nfc(&key) {
                        return Err(serde::de::Error::custom("non-NFC key"));
                    }
                    let value = map.next_value()?;
                    if values.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(CanonicalValue::Object(values))
            }
        }
        deserializer.deserialize_any(ValueVisitor)
    }
}

impl CanonicalValue {
    fn depth(&self) -> usize {
        match self {
            Self::Array(values) => 1 + values.iter().map(Self::depth).max().unwrap_or(0),
            Self::Object(values) => 1 + values.values().map(Self::depth).max().unwrap_or(0),
            _ => 0,
        }
    }
    fn into_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(v) => serde_json::Value::Bool(v),
            Self::Number(v) => serde_json::Value::Number(v),
            Self::String(v) => serde_json::Value::String(v),
            Self::Array(v) => {
                serde_json::Value::Array(v.into_iter().map(Self::into_json).collect())
            }
            Self::Object(v) => {
                serde_json::Value::Object(v.into_iter().map(|(k, v)| (k, v.into_json())).collect())
            }
        }
    }
}

pub fn validate_frame(frame: &[u8]) -> Result<WireOperation, TransportError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge);
    }
    let text = std::str::from_utf8(frame).map_err(|_| TransportError::InvalidUtf8)?;
    if text.chars().any(forbidden_control) {
        return Err(TransportError::UnicodeControlRejected);
    }
    let canonical: CanonicalValue = serde_json::from_str(text).map_err(|error| {
        let message = error.to_string();
        if message.contains("duplicate key") || message.contains("non-NFC") {
            TransportError::InvalidCanonicalEncoding
        } else {
            TransportError::InvalidSchema
        }
    })?;
    if canonical.depth() > 32 {
        return Err(TransportError::DepthExceeded);
    }
    let value = canonical.into_json();
    let encoded = serde_json_canonicalizer::to_vec(&value)
        .map_err(|_| TransportError::InvalidCanonicalEncoding)?;
    if encoded != frame {
        return Err(TransportError::InvalidCanonicalEncoding);
    }
    let operation: WireOperation =
        serde_json::from_value(value).map_err(|_| TransportError::InvalidSchema)?;
    validate_operation(&operation)?;
    Ok(operation)
}

fn validate_operation(operation: &WireOperation) -> Result<(), TransportError> {
    let validate_message = |id: &str| {
        if is_lower_hex(id, 32) {
            Ok(())
        } else {
            Err(TransportError::MessageIdInvalid)
        }
    };
    match operation {
        WireOperation::Send(frame) => {
            validate_message(&frame.message_id)?;
            if frame.grant_id.is_empty() {
                return Err(TransportError::InvalidSchema);
            }
            check_size(
                &frame.subject,
                MAX_SUBJECT_BYTES,
                TransportError::SubjectTooLarge,
            )?;
            check_size(&frame.body, MAX_BODY_BYTES, TransportError::BodyTooLarge)?;
            if let Some(description) = &frame.description {
                check_size(
                    description,
                    MAX_DESCRIPTION_BYTES,
                    TransportError::DescriptionTooLarge,
                )?;
            }
        }
        WireOperation::Edit(frame) => {
            validate_message(&frame.message_id)?;
            if frame.next_revision != frame.expected_revision.saturating_add(1) {
                return Err(TransportError::GrantRevisionGap);
            }
            check_size(
                &frame.subject,
                MAX_SUBJECT_BYTES,
                TransportError::SubjectTooLarge,
            )?;
            check_size(&frame.body, MAX_BODY_BYTES, TransportError::BodyTooLarge)?;
        }
        WireOperation::Receipt(frame) => validate_receipt(frame)?,
        WireOperation::AgentStop(frame) => {
            if frame.stop_grant_id.is_empty() || frame.target_generation == 0 {
                return Err(TransportError::InvalidSchema);
            }
        }
    }
    Ok(())
}

fn check_size(value: &str, max: usize, error: TransportError) -> Result<(), TransportError> {
    if value.len() > max {
        Err(error)
    } else {
        Ok(())
    }
}
fn is_nfc(value: &str) -> bool {
    value.nfc().eq(value.chars())
}
fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn forbidden_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        || (c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectConfigFile {
    mailbox: bool,
    #[serde(default)]
    inherit_mailbox_to_linked_worktrees: bool,
}

#[derive(Debug)]
struct VerifiedConfig {
    path: PathBuf,
    mailbox: bool,
    inherit: bool,
}

/// Resolve configuration from filesystem- and Git-derived provenance. Candidate lists
/// make ambiguity explicit; symlinks, owner mismatch, repository mismatch, malformed
/// Git pointers/config, and paths outside the exact checkout all fail closed.
#[allow(clippy::too_many_arguments)]
pub fn resolve_linked_config(
    expected_repository: &str,
    linked_checkout: &Path,
    linked_candidates: &[PathBuf],
    primary_checkout: &Path,
    primary_candidates: &[PathBuf],
) -> Result<Option<(bool, ConfigProvenance)>, TransportError> {
    if linked_candidates.len() > 1 || primary_candidates.len() > 1 {
        return Err(TransportError::ConfigAmbiguous);
    }
    if let Some(path) = linked_candidates.first() {
        let config = load_verified_config(path, expected_repository, linked_checkout, true)?;
        return Ok(Some((
            config.mailbox,
            ConfigProvenance::LinkedLocalOverride { path: config.path },
        )));
    }
    let Some(path) = primary_candidates.first() else {
        return Ok(None);
    };
    let config = load_verified_config(path, expected_repository, primary_checkout, false)?;
    if !config.inherit {
        return Ok(None);
    }
    Ok(Some((
        config.mailbox,
        ConfigProvenance::PrimaryExplicitInheritance { path: config.path },
    )))
}

fn load_verified_config(
    path: &Path,
    expected_repository: &str,
    checkout: &Path,
    expect_linked: bool,
) -> Result<VerifiedConfig, TransportError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, expected_repository, checkout, expect_linked);
        return Err(TransportError::ConfigProvenanceMismatch);
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

        let checkout = std::fs::canonicalize(checkout)
            .map_err(|_| TransportError::ConfigProvenanceMismatch)?;
        let space = crate::workspace::git_space_metadata(&checkout)
            .ok_or(TransportError::ConfigProvenanceMismatch)?;
        let repository = crate::repository::stable_repository_id(Path::new(&space.key));
        let discovered_checkout = std::fs::canonicalize(&space.checkout_key)
            .map_err(|_| TransportError::ConfigProvenanceMismatch)?;
        if repository != expected_repository
            || discovered_checkout != checkout
            || space.is_linked_worktree != expect_linked
        {
            return Err(TransportError::ConfigProvenanceMismatch);
        }

        // Resolve only the parent. The final component is opened once with
        // O_NOFOLLOW and all metadata/read operations use that same descriptor.
        let file_name = path
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or(TransportError::ConfigProvenanceMismatch)?;
        let parent = path
            .parent()
            .ok_or(TransportError::ConfigProvenanceMismatch)?;
        let parent =
            std::fs::canonicalize(parent).map_err(|_| TransportError::ConfigProvenanceMismatch)?;
        if !parent.starts_with(&checkout) {
            return Err(TransportError::ConfigProvenanceMismatch);
        }
        let verified_path = parent.join(file_name);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&verified_path)
            .map_err(|_| TransportError::ConfigProvenanceMismatch)?;
        let descriptor_metadata = file
            .metadata()
            .map_err(|_| TransportError::ConfigProvenanceMismatch)?;
        if !descriptor_metadata.is_file() || descriptor_metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(TransportError::ConfigProvenanceMismatch);
        }

        let descriptor_path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .map_err(|_| TransportError::ConfigProvenanceMismatch)?;
        if descriptor_path != verified_path || !descriptor_path.starts_with(&checkout) {
            return Err(TransportError::ConfigProvenanceMismatch);
        }
        verify_descriptor_still_named(&verified_path, &descriptor_metadata)?;

        let mut bytes = Vec::new();
        (&mut file)
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| TransportError::ConfigInvalid)?;
        if bytes.len() > 16 * 1024 {
            return Err(TransportError::ConfigInvalid);
        }
        // Detect rename/replacement during the read. The data itself always came
        // from the verified descriptor; a changed namespace is rejected.
        verify_descriptor_still_named(&verified_path, &descriptor_metadata)?;

        let parsed: DirectConfigFile =
            serde_json::from_slice(&bytes).map_err(|_| TransportError::ConfigInvalid)?;
        Ok(VerifiedConfig {
            path: verified_path,
            mailbox: parsed.mailbox,
            inherit: parsed.inherit_mailbox_to_linked_worktrees,
        })
    }
}

#[cfg(target_os = "linux")]
fn verify_descriptor_still_named(
    path: &Path,
    descriptor: &std::fs::Metadata,
) -> Result<(), TransportError> {
    use std::os::unix::fs::MetadataExt as _;

    let named =
        std::fs::symlink_metadata(path).map_err(|_| TransportError::ConfigProvenanceMismatch)?;
    if named.file_type().is_symlink()
        || !named.is_file()
        || named.dev() != descriptor.dev()
        || named.ino() != descriptor.ino()
        || named.uid() != descriptor.uid()
    {
        return Err(TransportError::ConfigProvenanceMismatch);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    FeatureDisabled,
    AuthorityUnconfirmed,
    GrantMissing,
    GrantCollision,
    GrantExpired,
    GrantRevoked,
    GrantRevisionGap,
    GrantRevisionRollback,
    GrantScopeMismatch,
    OperationNotAllowed,
    SkipLevelForbidden,
    LateralForbidden,
    AdvisoryOperationForbidden,
    AdvisoryLimit,
    StopGrantRequired,
    TopologyStale,
    SessionReplaced,
    MirrorDisconnected,
    FenceUnconfirmed,
    ChannelUnsupported,
    ChannelPeerMismatch,
    ChannelGenerationMismatch,
    ManifestMissing,
    ManifestStale,
    ManifestReplaced,
    RegistryGenerationExhausted,
    ProtocolIncompatible,
    FixtureDigestMismatch,
    ConfigInvalid,
    ConfigAmbiguous,
    ConfigProvenanceMismatch,
    CorrelationConflict,
    ReceiptMismatch,
    DeliveryAlreadyAdmitted,
    FrameTooLarge,
    InvalidSchema,
    InvalidCanonicalEncoding,
    InvalidUtf8,
    UnicodeControlRejected,
    DepthExceeded,
    MessageIdInvalid,
    SubjectTooLarge,
    BodyTooLarge,
    DescriptionTooLarge,
}

impl TransportError {
    pub fn code(self) -> &'static str {
        match self {
            Self::FeatureDisabled => "feature_disabled",
            Self::AuthorityUnconfirmed => "authority_unconfirmed",
            Self::GrantMissing => "grant_missing",
            Self::GrantCollision => "message_id_collision",
            Self::GrantExpired => "grant_expired",
            Self::GrantRevoked => "grant_revoked",
            Self::GrantRevisionGap => "grant_revision_gap",
            Self::GrantRevisionRollback => "grant_revision_rollback",
            Self::GrantScopeMismatch => "grant_scope_mismatch",
            Self::OperationNotAllowed => "operation_not_allowed",
            Self::SkipLevelForbidden => "skip_level_forbidden",
            Self::LateralForbidden => "lateral_forbidden",
            Self::AdvisoryOperationForbidden => "advisory_operation_forbidden",
            Self::AdvisoryLimit => "rate_limited",
            Self::StopGrantRequired => "stop_grant_required",
            Self::TopologyStale => "authority_unconfirmed",
            Self::SessionReplaced => "recipient_session_replaced",
            Self::MirrorDisconnected => "bridge_unavailable",
            Self::FenceUnconfirmed => "authority_unconfirmed",
            Self::ChannelUnsupported => "channel_unsupported",
            Self::ChannelPeerMismatch => "channel_peer_mismatch",
            Self::ChannelGenerationMismatch => "channel_generation_mismatch",
            Self::ManifestMissing => "manifest_missing",
            Self::ManifestStale => "manifest_stale",
            Self::ManifestReplaced => "manifest_replaced",
            Self::RegistryGenerationExhausted => "authority_unconfirmed",
            Self::ProtocolIncompatible => "protocol_incompatible",
            Self::FixtureDigestMismatch => "fixture_digest_mismatch",
            Self::ConfigInvalid => "config_invalid",
            Self::ConfigAmbiguous => "config_ambiguous",
            Self::ConfigProvenanceMismatch => "config_provenance_mismatch",
            Self::CorrelationConflict => "correlation_conflict",
            Self::ReceiptMismatch => "receipt_mismatch",
            Self::DeliveryAlreadyAdmitted => "delivery_already_admitted",
            Self::FrameTooLarge | Self::InvalidSchema => "invalid_schema",
            Self::InvalidCanonicalEncoding => "invalid_canonical_encoding",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::UnicodeControlRejected => "unicode_control_rejected",
            Self::DepthExceeded => "depth_exceeded",
            Self::MessageIdInvalid => "message_id_invalid",
            Self::SubjectTooLarge => "subject_too_large",
            Self::BodyTooLarge => "body_too_large",
            Self::DescriptionTooLarge => "description_too_large",
        }
    }
}
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for TransportError {}

#[derive(Debug, Clone, Copy, Default)]
pub struct DirectTransportConfig {
    pub enabled: bool,
}
impl DirectTransportConfig {
    pub fn require_enabled(self) -> Result<(), TransportError> {
        self.enabled
            .then_some(())
            .ok_or(TransportError::FeatureDisabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneId;

    fn id(raw: u64) -> DelegationId {
        DelegationId::from_raw(raw).unwrap()
    }
    fn principal(raw: u64, generation: u64) -> SessionGeneration {
        SessionGeneration {
            session: format!("session-{raw}"),
            generation,
            delegation_id: id(raw),
        }
    }
    fn scope() -> RouteScope {
        RouteScope {
            repository: "repo-1".into(),
            worktree: "/repo/wt".into(),
            branch: "feat/direct".into(),
        }
    }
    fn topology() -> CanonicalTopology {
        let records = [
            (70_001, 1, None),
            (70_002, 2, Some(70_001)),
            (70_003, 3, Some(70_002)),
            (70_004, 4, Some(70_001)),
        ]
        .map(|(raw, pane, parent)| crate::delegation::DelegationRecord {
            id: id(raw),
            pane_id: Some(PaneId::from_raw(pane)),
            parent_id: parent.map(id),
            purpose: None,
            sibling_rank: 0,
            tombstone: false,
        });
        let delegations = Delegations::from_records(records).unwrap();
        CanonicalTopology::from_persisted(
            &delegations,
            [70_001, 70_002, 70_003, 70_004]
                .into_iter()
                .map(|raw| PrincipalBinding {
                    identity: principal(raw, raw),
                    scope: scope(),
                }),
            9,
        )
        .unwrap()
    }
    fn channel(epoch: u64) -> ChannelBinding {
        ChannelBinding::verified_anonymous(
            format!("binding-{epoch}"),
            principal(70_002, 70_002),
            70_002,
            true,
        )
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_mailbox_context_uses_os_peer_not_request_identity() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::net::UnixStream;

        let (server, _client) = UnixStream::pair().unwrap();
        let recipient = principal(70_002, 70_002);
        let context = TrustedMailboxChannelContext::from_verified_local_socket(
            server.as_raw_fd(),
            recipient.clone(),
            "server-bound-channel".into(),
            std::process::id(),
            recipient.generation,
        )
        .expect("the OS peer is the current process in this socket-pair test");
        assert_eq!(context.binding().recipient(), &recipient);
        let mut manifests = ManifestRegistry::default();
        manifests
            .register_atomic(manifest(9, context.binding()), context.binding(), 9)
            .unwrap();
        let route = context.negotiate(&manifests, 9).unwrap();
        assert_eq!(route.identity().recipient(), &recipient);

        // Spoofing a recipient generation in a request cannot affect the already-verified
        // binding; a mismatched server lifecycle generation fails before route negotiation.
        assert_eq!(
            TrustedMailboxChannelContext::from_verified_local_socket(
                server.as_raw_fd(),
                recipient,
                "server-bound-channel".into(),
                std::process::id(),
                999,
            )
            .unwrap_err(),
            TransportError::ChannelGenerationMismatch
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_mailbox_context_rejects_a_spoofed_foreground_identity() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::net::UnixStream;

        let (server, _client) = UnixStream::pair().unwrap();
        assert_eq!(
            TrustedMailboxChannelContext::from_verified_local_socket(
                server.as_raw_fd(),
                principal(70_002, 70_002),
                "server-bound-channel".into(),
                u32::MAX,
                70_002,
            )
            .unwrap_err(),
            TransportError::ChannelPeerMismatch
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recipient_registry_derives_routes_only_from_registered_trusted_context() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::net::UnixStream;

        let (server, _client) = UnixStream::pair().unwrap();
        let recipient = principal(70_002, 70_002);
        let context = TrustedMailboxChannelContext::from_verified_local_socket(
            server.as_raw_fd(),
            recipient.clone(),
            "registered-binding".into(),
            std::process::id(),
            70_002,
        )
        .unwrap();
        let mut registry = RecipientChannelRegistry::default();
        registry
            .register(context, manifest(9, &channel(70_002)), topology(), 9)
            .unwrap_err();

        // A manifest whose binding comes from another source cannot register. Build the
        // matching manifest from the trusted context and prove discovery yields that route.
        let context = TrustedMailboxChannelContext::from_verified_local_socket(
            server.as_raw_fd(),
            recipient.clone(),
            "registered-binding".into(),
            std::process::id(),
            70_002,
        )
        .unwrap();
        let matching_manifest = manifest(9, context.binding());
        registry
            .register(context, matching_manifest, topology(), 9)
            .unwrap();
        let info = registry.registered(&recipient).unwrap();
        assert_eq!(info.recipient, recipient);
        assert_eq!(info.binding_id, "registered-binding");
        assert_eq!(info.foreground_pi_pid, std::process::id());
        registry
            .with_authenticated_route(
                &info.recipient,
                10,
                |context, _manifests, topology, route| {
                    assert_eq!(context.binding().id(), "registered-binding");
                    assert_eq!(topology.revision(), 9);
                    assert_eq!(route.identity().recipient(), &info.recipient);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recipient_registry_rejects_stale_lifecycle_invalidation() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::net::UnixStream;

        let (server, _client) = UnixStream::pair().unwrap();
        let recipient = principal(70_002, 70_002);
        let context = TrustedMailboxChannelContext::from_verified_local_socket(
            server.as_raw_fd(),
            recipient.clone(),
            "live-binding".into(),
            std::process::id(),
            70_002,
        )
        .unwrap();
        let manifest = manifest(9, context.binding());
        let mut registry = RecipientChannelRegistry::default();
        registry.register(context, manifest, topology(), 9).unwrap();
        assert_eq!(
            registry.invalidate(&recipient, "stale-binding"),
            Err(TransportError::ChannelPeerMismatch)
        );
        registry.invalidate(&recipient, "live-binding").unwrap();
        assert_eq!(
            registry.with_authenticated_route(
                &recipient,
                10,
                |_context, _manifests, _topology, _route| Ok(())
            ),
            Err(TransportError::ManifestMissing)
        );
    }
    fn manifest(epoch: u64, channel: &ChannelBinding) -> LiveManifest {
        LiveManifest {
            recipient: channel.recipient().clone(),
            protocol_min: 1,
            protocol_max: 1,
            fixture_digest: FIXTURE_DIGEST.into(),
            features: HashSet::from(["direct-v1".into()]),
            gate_mailbox_version: 1,
            package_revision: "abc123".into(),
            effective_config_digest: "a".repeat(64),
            config_provenance: ConfigProvenance::LinkedLocalOverride {
                path: "/repo/wt/config.json".into(),
            },
            registration_epoch: epoch,
            expires_at: 1000,
            channel_binding: channel.id().into(),
        }
    }
    fn registry(epoch: u64) -> (ManifestRegistry, ChannelBinding, NegotiatedRoute) {
        let channel = channel(epoch);
        let mut registry = ManifestRegistry::default();
        registry
            .register_atomic(manifest(epoch, &channel), &channel, epoch)
            .unwrap();
        let route = registry
            .negotiate(&channel, 1, FIXTURE_DIGEST, 1, epoch)
            .unwrap();
        (registry, channel, route)
    }
    fn standard_request(grant_id: &str, revision: u64, route: NegotiatedRoute) -> GrantRequest {
        GrantRequest {
            grant_id: grant_id.into(),
            task_id: "todo-10".into(),
            delegation_id: id(70_002),
            edge_kind: EdgeKind::PmTpm,
            class: GrantClass::Standard,
            issuer: principal(70_001, 70_001),
            recipient: principal(70_002, 70_002),
            authorized_by: None,
            scope: scope(),
            message_kinds: HashSet::from([MessageKind::Assignment, MessageKind::Question]),
            effects: HashSet::from([Effect::Send, Effect::Edit]),
            issued_at: 100,
            expires_at: 900,
            topology_revision: 9,
            grant_revision: revision,
            route,
        }
    }
    fn context(revision: u64) -> AuthorityContext {
        AuthorityContext {
            issuer: principal(70_001, 70_001),
            recipient: principal(70_002, 70_002),
            scope: scope(),
            topology_revision: 9,
            grant_revision: revision,
            connected: true,
            authority_confirmed: true,
        }
    }

    #[test]
    fn default_off_and_fixture_digest_are_frozen() {
        assert_eq!(
            DirectTransportConfig::default().require_enabled(),
            Err(TransportError::FeatureDisabled)
        );
        assert_eq!(
            FIXTURE_DIGEST,
            "16d169e6c9b472b864c2c97e6d77c9268dc98d0d68c75154fa23ed87d2729129"
        );
    }

    #[test]
    fn verified_channel_construction_rejects_child_inheritance_peer_and_generation_forgery() {
        assert_eq!(
            ChannelBinding::verified_anonymous("a".into(), principal(70_002, 8), 8, false),
            Err(TransportError::ChannelGenerationMismatch)
        );
        assert_eq!(
            ChannelBinding::verified_local_socket(
                "s".into(),
                principal(70_002, 8),
                true,
                1001,
                1000,
                42,
                40,
                true,
                8
            ),
            Err(TransportError::ChannelPeerMismatch)
        );
        assert_eq!(
            ChannelBinding::verified_local_socket(
                "s".into(),
                principal(70_002, 8),
                true,
                1000,
                1000,
                42,
                40,
                false,
                8
            ),
            Err(TransportError::ChannelPeerMismatch)
        );
        assert_eq!(
            ChannelBinding::verified_local_socket(
                "s".into(),
                principal(70_002, 8),
                true,
                1000,
                1000,
                42,
                40,
                true,
                7
            ),
            Err(TransportError::ChannelGenerationMismatch)
        );
    }

    #[test]
    fn manifest_replacement_is_monotonic_and_invalidates_routes_and_tickets() {
        let (mut manifests, first_channel, first_route) = registry(100);
        let stale_channel = channel(99);
        assert_eq!(
            manifests.register_atomic(manifest(99, &stale_channel), &stale_channel, 100),
            Err(TransportError::ManifestReplaced)
        );
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        authority
            .issue(
                standard_request("g", 1, first_route.clone()),
                &topology,
                &manifests,
                100,
            )
            .unwrap();
        let ticket = authority
            .ticket(
                "g",
                &context(1),
                &first_route,
                &manifests,
                Effect::Send,
                Some(MessageKind::Assignment),
                101,
            )
            .unwrap();
        let next_channel = channel(101);
        manifests
            .register_atomic(manifest(101, &next_channel), &next_channel, 101)
            .unwrap();
        assert_eq!(
            manifests.validate_ticket(&ticket, 102),
            Err(TransportError::ManifestReplaced)
        );
        assert_eq!(
            authority.ticket(
                "g",
                &context(1),
                &first_route,
                &manifests,
                Effect::Send,
                Some(MessageKind::Assignment),
                102
            ),
            Err(TransportError::ManifestReplaced)
        );
        assert_ne!(first_channel.id(), next_channel.id());
    }

    #[test]
    fn canonical_edges_reject_collision_skip_level_lateral_and_confused_deputy() {
        let (manifests, _, route) = registry(100);
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        authority
            .issue(
                standard_request("g", 1, route.clone()),
                &topology,
                &manifests,
                100,
            )
            .unwrap();
        assert_eq!(
            authority.issue(
                standard_request("g", 2, route.clone()),
                &topology,
                &manifests,
                100
            ),
            Err(TransportError::GrantCollision)
        );
        let mut skip = standard_request("skip", 2, route.clone());
        skip.recipient = principal(70_003, 70_003);
        skip.delegation_id = id(70_003);
        assert_eq!(
            authority.issue(skip, &topology, &manifests, 100),
            Err(TransportError::GrantScopeMismatch)
        );
        let mut deputy = context(1);
        deputy.issuer = principal(70_004, 70_004);
        assert_eq!(
            authority.ticket(
                "g",
                &deputy,
                &route,
                &manifests,
                Effect::Send,
                Some(MessageKind::Assignment),
                101
            ),
            Err(TransportError::SessionReplaced)
        );
    }

    struct MockGate {
        acknowledge: bool,
    }
    impl GateFenceInstaller for MockGate {
        fn install_revocation_fence(
            &mut self,
            f: &RevocationFence,
        ) -> Result<FenceReceipt, TransportError> {
            Ok(FenceReceipt {
                grant_id: f.grant_id.clone(),
                revoked_revision: f.revoked_revision,
                recipient: f.recipient.clone(),
                installed: self.acknowledge,
            })
        }
    }

    #[test]
    fn revocation_blocks_immediately_survives_interleaved_revision_and_requires_fence() {
        let (manifests, _, route) = registry(100);
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        authority
            .issue(
                standard_request("a", 1, route.clone()),
                &topology,
                &manifests,
                100,
            )
            .unwrap();
        let fence = authority.begin_revocation("a", 2).unwrap();
        authority
            .issue(
                standard_request("b", 3, route.clone()),
                &topology,
                &manifests,
                100,
            )
            .unwrap();
        assert_eq!(
            authority.ticket(
                "a",
                &context(3),
                &route,
                &manifests,
                Effect::Send,
                Some(MessageKind::Assignment),
                101
            ),
            Err(TransportError::GrantRevoked)
        );
        authority
            .complete_revocation(FenceReceipt {
                grant_id: "a".into(),
                revoked_revision: fence.revoked_revision,
                recipient: fence.recipient,
                installed: true,
            })
            .unwrap();
        assert_eq!(authority.status("a"), Some(GrantStatus::Revoked));
    }

    #[test]
    fn failed_mock_gate_fence_leaves_revoking_fail_closed() {
        let (manifests, _, route) = registry(100);
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        authority
            .issue(standard_request("a", 1, route), &topology, &manifests, 100)
            .unwrap();
        assert_eq!(
            authority.revoke_with_gate("a", 2, &mut MockGate { acknowledge: false }),
            Err(TransportError::FenceUnconfirmed)
        );
        assert_eq!(authority.status("a"), Some(GrantStatus::Revoking));
    }

    #[test]
    fn bounded_advisory_is_common_ancestor_typed_bounded_and_expiry_does_not_consume_quota() {
        let advisory_channel = ChannelBinding::verified_anonymous(
            "advisory-binding".into(),
            principal(70_004, 70_004),
            70_004,
            true,
        )
        .unwrap();
        let mut manifests = ManifestRegistry::default();
        manifests
            .register_atomic(manifest(100, &advisory_channel), &advisory_channel, 100)
            .unwrap();
        let route = manifests
            .negotiate(&advisory_channel, 1, FIXTURE_DIGEST, 1, 100)
            .unwrap();
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        let mut request = standard_request("advisory", 1, route.clone());
        request.edge_kind = EdgeKind::BoundedPeerAdvisory;
        request.class = GrantClass::Advisory;
        request.issuer = principal(70_002, 70_002);
        request.recipient = principal(70_004, 70_004);
        request.delegation_id = id(70_004);
        request.authorized_by = Some(principal(70_001, 70_001));
        request.message_kinds = HashSet::from([MessageKind::Advisory]);
        request.effects = HashSet::from([Effect::Send]);
        request.expires_at = 150;
        authority
            .issue(request, &topology, &manifests, 100)
            .unwrap();
        let mut next = standard_request("advisory-next", 2, route);
        next.edge_kind = EdgeKind::BoundedPeerAdvisory;
        next.class = GrantClass::Advisory;
        next.issuer = principal(70_002, 70_002);
        next.recipient = principal(70_004, 70_004);
        next.delegation_id = id(70_004);
        next.authorized_by = Some(principal(70_001, 70_001));
        next.message_kinds = HashSet::from([MessageKind::Advisory]);
        next.effects = HashSet::from([Effect::Send]);
        next.issued_at = 200;
        next.expires_at = 250;
        authority.issue(next, &topology, &manifests, 200).unwrap();
    }

    #[test]
    fn stop_only_uses_fixed_reason_and_exact_target() {
        let (manifests, _, route) = registry(100);
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        let mut request = standard_request("stop", 1, route.clone());
        request.class = GrantClass::StopOnly;
        request.message_kinds.clear();
        request.effects = HashSet::from([Effect::AgentStop]);
        authority
            .issue(request, &topology, &manifests, 100)
            .unwrap();
        let ticket = authority
            .ticket(
                "stop",
                &context(1),
                &route,
                &manifests,
                Effect::AgentStop,
                None,
                101,
            )
            .unwrap();
        authorize_stop(
            &ticket,
            &principal(70_002, 70_002),
            StopReason::ImminentUnsafeEffect,
        )
        .unwrap();
    }

    fn delivery(correlation: char) -> DeliveryRecord {
        DeliveryRecord {
            message_id: "a".repeat(32),
            correlation_id: correlation.to_string().repeat(32),
            payload_digest: "d".repeat(64),
            recipient: principal(70_002, 70_002),
            created_at: 100,
            transport_accepted_at: None,
            resolution: None,
        }
    }

    fn admission_receipt(correlation: char) -> ReceiptFrame {
        ReceiptFrame {
            message_id: "a".repeat(32),
            correlation_id: correlation.to_string().repeat(32),
            payload_digest: "d".repeat(64),
            recipient: principal(70_002, 70_002),
            outcome: AdmissionOutcome::Admitted {
                admission_revision: 9,
            },
        }
    }

    #[test]
    fn transport_acceptance_and_pre_admission_drop_preserve_sender_uncertainty() {
        let mut journal = SenderDeliveryJournal::default();
        journal.begin(delivery('b')).unwrap();
        journal
            .record_transport_acceptance(&"b".repeat(32), 101)
            .unwrap();

        assert_eq!(journal.unresolved().count(), 1);
        journal.retain_from(1000);
        assert_eq!(journal.unresolved().count(), 1);
    }

    #[test]
    fn stale_config_rejection_survives_reload_and_uses_fresh_correlation_for_retry() {
        let mut journal = SenderDeliveryJournal::default();
        journal.begin(delivery('b')).unwrap();
        let rejected = ReceiptFrame {
            outcome: AdmissionOutcome::Rejected {
                code: "stale_config".into(),
            },
            ..admission_receipt('b')
        };
        journal.reconcile(&rejected).unwrap();

        let persisted = serde_json::to_vec(&journal.snapshot()).unwrap();
        let snapshot = serde_json::from_slice(&persisted).unwrap();
        let mut recovered = SenderDeliveryJournal::restore(snapshot).unwrap();
        recovered
            .begin_retry(&"b".repeat(32), "c".repeat(32), 102)
            .unwrap();
        assert_eq!(recovered.unresolved().count(), 1);
    }

    #[test]
    fn duplicate_safe_retry_admission_reconciles_all_attempts_without_replay() {
        let mut journal = SenderDeliveryJournal::default();
        journal.begin(delivery('b')).unwrap();
        journal
            .record_transport_acceptance(&"b".repeat(32), 101)
            .unwrap();
        journal
            .begin_retry(&"b".repeat(32), "c".repeat(32), 102)
            .unwrap();

        let receipt = admission_receipt('c');
        journal.reconcile(&receipt).unwrap();
        journal.reconcile(&receipt).unwrap();
        assert_eq!(journal.unresolved().count(), 0);
        assert_eq!(journal.snapshot().records.len(), 2);
        assert_eq!(
            journal.begin_retry(&"c".repeat(32), "e".repeat(32), 103),
            Err(TransportError::DeliveryAlreadyAdmitted)
        );
    }

    #[test]
    fn mismatched_or_conflicting_receipts_cannot_resolve_pending_delivery() {
        let mut journal = SenderDeliveryJournal::default();
        journal.begin(delivery('b')).unwrap();
        let mut mismatched = admission_receipt('b');
        mismatched.payload_digest = "e".repeat(64);
        assert_eq!(
            journal.reconcile(&mismatched),
            Err(TransportError::ReceiptMismatch)
        );
        assert_eq!(journal.unresolved().count(), 1);

        journal.reconcile(&admission_receipt('b')).unwrap();
        let conflicting = ReceiptFrame {
            outcome: AdmissionOutcome::Rejected {
                code: "stale_config".into(),
            },
            ..admission_receipt('b')
        };
        assert_eq!(
            journal.reconcile(&conflicting),
            Err(TransportError::CorrelationConflict)
        );
    }

    #[test]
    fn admission_receipt_is_bound_to_full_recipient_identity() {
        let mut journal = SenderDeliveryJournal::default();
        journal.begin(delivery('b')).unwrap();
        let mut other = delivery('c');
        other.recipient = principal(70_004, 70_002);
        journal.begin(other).unwrap();

        journal.reconcile(&admission_receipt('b')).unwrap();
        let unresolved: Vec<_> = journal.unresolved().collect();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].correlation_id, "c".repeat(32));
        assert_eq!(unresolved[0].recipient, principal(70_004, 70_002));
    }

    #[test]
    fn reconciliation_and_restore_reject_invalid_admission_outcomes() {
        let mut journal = SenderDeliveryJournal::default();
        journal.begin(delivery('b')).unwrap();
        let zero_revision = ReceiptFrame {
            outcome: AdmissionOutcome::Admitted {
                admission_revision: 0,
            },
            ..admission_receipt('b')
        };
        assert_eq!(
            journal.reconcile(&zero_revision),
            Err(TransportError::InvalidSchema)
        );
        let invalid_rejection = ReceiptFrame {
            outcome: AdmissionOutcome::Rejected {
                code: "STALE-CONFIG".into(),
            },
            ..admission_receipt('b')
        };
        assert_eq!(
            journal.reconcile(&invalid_rejection),
            Err(TransportError::InvalidSchema)
        );
        assert_eq!(journal.unresolved().count(), 1);

        let mut invalid_record = delivery('c');
        invalid_record.resolution = Some(AdmissionOutcome::Admitted {
            admission_revision: 0,
        });
        assert!(matches!(
            SenderDeliveryJournal::restore(SenderDeliverySnapshot {
                records: vec![invalid_record]
            }),
            Err(TransportError::InvalidSchema)
        ));
    }

    #[test]
    fn canonical_typed_frames_reject_duplicates_noncanonical_nfc_schema_ids_sizes_and_string_brackets(
    ) {
        let valid = format!(
            r#"{{"body":"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[","grantId":"g","kind":"assignment","messageId":"{}","operation":"send","priority":"normal","subject":"s"}}"#,
            "a".repeat(32)
        );
        assert!(matches!(
            validate_frame(valid.as_bytes()),
            Ok(WireOperation::Send(_))
        ));
        assert_eq!(validate_frame(b"null"), Err(TransportError::InvalidSchema));
        assert_eq!(validate_frame(br#"{"operation":"receipt","messageId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","messageId":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#),Err(TransportError::InvalidCanonicalEncoding));
        assert_eq!(
            validate_frame(
                br#"{ "operation":"receipt","messageId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
            ),
            Err(TransportError::InvalidCanonicalEncoding)
        );
        assert_eq!(
            validate_frame(
                "{\"operation\":\"receipt\",\"messageId\":\"éééééééééééééééé\"}"
                    .nfd()
                    .collect::<String>()
                    .as_bytes()
            ),
            Err(TransportError::InvalidCanonicalEncoding)
        );
        let oversized = format!(
            r#"{{"body":"b","grantId":"g","kind":"assignment","messageId":"{}","operation":"send","priority":"normal","subject":"{}"}}"#,
            "a".repeat(32),
            "s".repeat(161)
        );
        assert_eq!(
            validate_frame(oversized.as_bytes()),
            Err(TransportError::SubjectTooLarge)
        );
        assert_eq!(
            validate_frame(&vec![b'a'; MAX_FRAME_BYTES + 1]),
            Err(TransportError::FrameTooLarge)
        );
    }

    #[test]
    fn manifest_digest_must_be_lowercase_sha256_and_disconnect_removes_route() {
        let initial_channel = channel(100);
        let mut bad = manifest(100, &initial_channel);
        bad.effective_config_digest = "Z".repeat(64);
        let mut registry = ManifestRegistry::default();
        assert_eq!(
            registry.register_atomic(bad, &initial_channel, 100),
            Err(TransportError::ProtocolIncompatible)
        );
        registry
            .register_atomic(manifest(100, &initial_channel), &initial_channel, 100)
            .unwrap();
        let route = registry
            .negotiate(&initial_channel, 1, FIXTURE_DIGEST, 1, 100)
            .unwrap();
        registry.disconnect(initial_channel.id()).unwrap();
        assert_eq!(
            registry.validate_route(&route, 101),
            Err(TransportError::ManifestMissing)
        );

        let stale_after_disconnect = channel(99);
        assert_eq!(
            registry.register_atomic(
                manifest(99, &stale_after_disconnect),
                &stale_after_disconnect,
                101
            ),
            Err(TransportError::ManifestReplaced)
        );
        let newer = channel(101);
        registry
            .register_atomic(manifest(101, &newer), &newer, 101)
            .unwrap();
        registry.reload().unwrap();
        let stale_after_reload = channel(100);
        assert_eq!(
            registry.register_atomic(manifest(100, &stale_after_reload), &stale_after_reload, 102),
            Err(TransportError::ManifestReplaced)
        );
    }

    #[test]
    fn registry_generation_exhaustion_fails_without_mutating_live_manifest() {
        let channel = channel(100);
        let mut registry = ManifestRegistry::default();
        registry
            .register_atomic(manifest(100, &channel), &channel, 100)
            .unwrap();
        registry.generation = u64::MAX;
        assert_eq!(
            registry.disconnect(channel.id()),
            Err(TransportError::RegistryGenerationExhausted)
        );
        assert!(registry.current.is_some());
        assert_eq!(
            registry.reload(),
            Err(TransportError::RegistryGenerationExhausted)
        );
        assert!(registry.current.is_some());

        let mut exhausted_registration = ManifestRegistry {
            generation: u64::MAX,
            ..ManifestRegistry::default()
        };
        assert_eq!(
            exhausted_registration.register_atomic(manifest(100, &channel), &channel, 100),
            Err(TransportError::RegistryGenerationExhausted)
        );
        assert!(exhausted_registration.current.is_none());
        assert_eq!(exhausted_registration.registration_epoch_high_water, 0);
    }

    #[test]
    fn topology_replacement_is_monotonic_and_invalidates_existing_grants() {
        let (manifests, _, route) = registry(100);
        let topology = topology();
        let mut authority = GrantAuthority::new(9);
        authority
            .issue(standard_request("g", 1, route), &topology, &manifests, 100)
            .unwrap();
        assert_eq!(
            authority.replace_topology(9, true),
            Err(TransportError::GrantRevisionRollback)
        );
        authority.replace_topology(10, true).unwrap();
        assert_eq!(authority.status("g"), Some(GrantStatus::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn config_provenance_is_derived_from_filesystem_git_owner_and_exact_checkout() {
        use std::os::unix::fs::symlink;
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "herdr-direct-config-{}-{nonce}",
            std::process::id()
        ));
        let primary = base.join("primary");
        let linked = base.join("linked");
        std::fs::create_dir_all(&primary).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&primary, &["init", "--quiet"]);
        git(&primary, &["config", "user.email", "test@example.invalid"]);
        git(&primary, &["config", "user.name", "Test"]);
        std::fs::write(primary.join("seed"), "seed").unwrap();
        git(&primary, &["add", "seed"]);
        git(&primary, &["commit", "--quiet", "-m", "seed"]);
        git(
            &primary,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feat/config-test",
                linked.to_str().unwrap(),
            ],
        );

        let space = crate::workspace::git_space_metadata(&primary).unwrap();
        let repository = crate::repository::stable_repository_id(Path::new(&space.key));
        let linked_config = linked.join("direct.json");
        let primary_config = primary.join("direct.json");
        std::fs::write(&linked_config, r#"{"mailbox":true}"#).unwrap();
        assert!(matches!(
            resolve_linked_config(
                &repository,
                &linked,
                std::slice::from_ref(&linked_config),
                &primary,
                &[]
            )
            .unwrap(),
            Some((true, ConfigProvenance::LinkedLocalOverride { .. }))
        ));

        std::fs::remove_file(&linked_config).unwrap();
        std::fs::write(
            &primary_config,
            r#"{"mailbox":true,"inheritMailboxToLinkedWorktrees":true}"#,
        )
        .unwrap();
        assert!(matches!(
            resolve_linked_config(
                &repository,
                &linked,
                &[],
                &primary,
                std::slice::from_ref(&primary_config)
            )
            .unwrap(),
            Some((true, ConfigProvenance::PrimaryExplicitInheritance { .. }))
        ));
        symlink(&primary_config, &linked_config).unwrap();
        assert_eq!(
            resolve_linked_config(
                &repository,
                &linked,
                std::slice::from_ref(&linked_config),
                &primary,
                &[]
            ),
            Err(TransportError::ConfigProvenanceMismatch)
        );
        assert_eq!(
            resolve_linked_config(
                &repository,
                &linked,
                &[linked_config.clone(), linked_config],
                &primary,
                &[]
            ),
            Err(TransportError::ConfigAmbiguous)
        );
        assert_eq!(
            resolve_linked_config(
                "wrong-repository",
                &linked,
                &[],
                &primary,
                &[primary_config]
            ),
            Err(TransportError::ConfigProvenanceMismatch)
        );

        git(
            &primary,
            &["worktree", "remove", "--force", linked.to_str().unwrap()],
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn public_debug_state_has_no_bearer_material() {
        let (_, channel, route) = registry(100);
        let rendered = format!("{channel:?}{route:?}");
        for forbidden in [
            "reusable_secret",
            "capability",
            "route_nonce",
            "transport_marker",
            "message_body",
            "human_draft",
        ] {
            assert!(!rendered.contains(forbidden));
        }
    }
}
