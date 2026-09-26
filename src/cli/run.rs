use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

use crate::api::schema::{
    AgentInfo, AgentSessionInfo, AgentTarget, ArtifactRef, CanonicalHerdrIdentity,
    CollectionCreateMemberParams, HandoffKind, HandoffSendParams, HandoffTransportOutcome,
    HerdrHandoff, Method, Request, WorktreeBranchMode, WorktreeCreateParams, WorktreeOpenParams,
};

#[derive(Default)]
struct RunArgs {
    repo: Option<PathBuf>,
    base: Option<String>,
    branch: Option<String>,
    existing: Option<PathBuf>,
    label: Option<String>,
    role: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    thinking: Option<String>,
    complexity_reason: Option<String>,
    assignment_file: Option<PathBuf>,
    parent: Option<String>,
    name: Option<String>,
    collection: Option<String>,
    profile_helper: Option<PathBuf>,
    timeout_ms: u64,
    cleanup_on_failure: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum StageOutcome {
    Created,
    Reused,
    Verified,
    Unverified,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
struct RunStage {
    name: &'static str,
    outcome: StageOutcome,
    detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunReceipt {
    version: u8,
    run_id: String,
    created_at: String,
    parent: Option<CanonicalHerdrIdentity>,
    repository: Option<RunRepository>,
    workspace_id: Option<String>,
    pane_id: Option<String>,
    terminal_id: Option<String>,
    agent_session: Option<AgentSessionInfo>,
    role_profile: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    complexity_reason: Option<String>,
    assignment_digest: Option<String>,
    stages: Vec<RunStage>,
    created_resources: Vec<String>,
    error: Option<String>,
    cleanup_requested: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunRepository {
    root: String,
    base_revision: String,
    branch: Option<String>,
    worktree_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dirty_status_digest: Option<String>,
}

impl RunReceipt {
    fn new(run_id: String, args: &RunArgs) -> Self {
        Self {
            version: 1,
            created_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("current UTC time formats as RFC 3339"),
            run_id,
            parent: None,
            repository: None,
            workspace_id: None,
            pane_id: None,
            terminal_id: None,
            agent_session: None,
            role_profile: args.role.clone(),
            provider: args.provider.clone(),
            model: args.model.clone(),
            thinking: args.thinking.clone(),
            complexity_reason: args.complexity_reason.clone(),
            assignment_digest: None,
            stages: vec![],
            created_resources: vec![],
            error: None,
            cleanup_requested: args.cleanup_on_failure,
        }
    }
    fn stage(&mut self, name: &'static str, outcome: StageOutcome, detail: impl Into<String>) {
        self.stages.push(RunStage {
            name,
            outcome,
            detail: detail.into(),
        });
    }
    fn fail(&mut self, name: &'static str, detail: impl Into<String>) -> i32 {
        let detail = detail.into();
        self.stage(name, StageOutcome::Failed, detail.clone());
        self.error = Some(detail);
        1
    }
}

pub(super) fn run_run_command(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_args(args) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{err}");
            print_help();
            return Ok(2);
        }
    };
    let run_id = format!("run-{}-{}", unix_seconds(), std::process::id());
    let mut receipt = RunReceipt::new(run_id, &parsed);
    let code = execute(&parsed, &mut receipt);
    println!(
        "{}",
        serde_json::to_string(&receipt).expect("run receipt serializes")
    );
    Ok(code)
}

fn execute(args: &RunArgs, receipt: &mut RunReceipt) -> i32 {
    let Some(profile_helper) = args
        .profile_helper
        .clone()
        .or_else(|| std::env::var_os("HERDR_PANE_PROFILE_HELPER").map(PathBuf::from))
    else {
        return receipt.fail(
            "validate_inputs",
            "external profile interface unavailable; set --profile-helper or HERDR_PANE_PROFILE_HELPER (see docs/handoff-and-run.md)",
        );
    };
    let parent = match read_json_arg::<CanonicalHerdrIdentity>(
        args.parent.as_deref().expect("validated args"),
    ) {
        Ok(value) => value,
        Err(err) => {
            return receipt.fail("validate_inputs", format!("invalid parent identity: {err}"))
        }
    };
    receipt.parent = Some(parent.clone());
    let parent_response = match super::send_request(&Request {
        id: format!("cli:{}:parent", receipt.run_id),
        method: Method::AgentGet(AgentTarget {
            target: parent.pane_id.clone(),
        }),
    }) {
        Ok(value) => value,
        Err(err) => return receipt.fail("validate_parent", err.to_string()),
    };
    if let Some(err) = response_error(&parent_response) {
        return receipt.fail("validate_parent", err);
    }
    let current_parent: AgentInfo =
        match serde_json::from_value(parent_response["result"]["agent"].clone()) {
            Ok(value) => value,
            Err(err) => {
                return receipt.fail(
                    "validate_parent",
                    format!("invalid parent lookup response: {err}"),
                )
            }
        };
    if identity_from_agent_info(&current_parent).as_ref() != Some(&parent) {
        return receipt.fail(
            "validate_parent",
            "parent identity no longer matches its exact current agent session",
        );
    }
    receipt.stage(
        "validate_parent",
        StageOutcome::Verified,
        parent.pane_id.clone(),
    );
    let assignment =
        match std::fs::read_to_string(args.assignment_file.as_ref().expect("validated args")) {
            Ok(value) => value,
            Err(err) => {
                return receipt.fail("validate_inputs", format!("cannot read assignment: {err}"))
            }
        };
    if assignment.is_empty()
        || assignment.len() > crate::api::schema::MAX_SUMMARY_BYTES
        || assignment.chars().any(forbidden_assignment_char)
    {
        return receipt.fail("validate_inputs", "assignment must be non-empty, bounded to 2048 UTF-8 bytes, and free of terminal/bidi controls");
    }
    receipt.assignment_digest = Some(format!(
        "sha256:{:x}",
        Sha256::digest(assignment.as_bytes())
    ));

    let repo = match canonical_repo(args.repo.as_ref().expect("validated args")) {
        Ok(value) => value,
        Err(err) => return receipt.fail("resolve_repository", err),
    };
    let base = match git_output(
        &repo,
        &[
            "rev-parse",
            "--verify",
            &format!(
                "{}^{{commit}}",
                args.base.as_deref().expect("validated args")
            ),
        ],
    ) {
        Ok(value) => value,
        Err(err) => return receipt.fail("resolve_repository", err),
    };
    receipt.stage(
        "resolve_repository",
        StageOutcome::Verified,
        format!("{} at {base}", repo.display()),
    );

    let launch = if let Some(branch) = &args.branch {
        if args.collection.is_some() {
            return receipt.fail(
                "create_resource",
                "new-branch and collection launch modes are mutually exclusive",
            );
        }
        let target = match conventional_worktree_path(&repo, branch) {
            Ok(value) => value,
            Err(err) => return receipt.fail("create_resource", err),
        };
        let response = match super::send_request(&Request {
            id: format!("cli:{}:worktree", receipt.run_id),
            method: Method::WorktreeCreate(WorktreeCreateParams {
                cwd: Some(repo.display().to_string()),
                branch: Some(branch.clone()),
                branch_mode: WorktreeBranchMode::NewOnly,
                base: Some(base.clone()),
                path: Some(target.display().to_string()),
                label: args.label.clone(),
                focus: false,
                repository_id: None,
                workspace_id: None,
                trust_repository: false,
            }),
        }) {
            Ok(value) => value,
            Err(err) => {
                return finish_failure_with_cleanup(
                    args,
                    receipt,
                    "create_resource",
                    format!("ambiguous worktree create transport failure: {err}"),
                )
            }
        };
        if let Some(err) = response_error(&response) {
            return finish_failure_with_cleanup(args, receipt, "create_resource", err);
        }
        receipt.repository = Some(RunRepository {
            root: repo.display().to_string(),
            base_revision: base.clone(),
            branch: Some(branch.clone()),
            worktree_path: target.display().to_string(),
            dirty_status_digest: None,
        });
        receipt
            .created_resources
            .push(format!("worktree:{}", target.display()));
        receipt.created_resources.push(format!(
            "workspace:{}",
            response["result"]["workspace"]["id"]
                .as_str()
                .unwrap_or("unknown")
        ));
        receipt.stage(
            "create_resource",
            StageOutcome::Created,
            target.display().to_string(),
        );
        launch_from_response(&response)
    } else {
        let checkout = match canonical_existing_checkout(
            &repo,
            args.existing.as_ref().expect("validated args"),
        ) {
            Ok(value) => value,
            Err(err) => return receipt.fail("validate_existing", err),
        };
        let checkout_head = match git_output(&checkout, &["rev-parse", "HEAD"]) {
            Ok(value) => value,
            Err(err) => return receipt.fail("validate_existing", err),
        };
        if checkout_head != base {
            return receipt.fail(
                "validate_existing",
                format!(
                    "existing checkout HEAD {checkout_head} does not match requested base {base}"
                ),
            );
        }
        let dirty = match git_output_bytes(&checkout, &["status", "--porcelain=v1", "-z"]) {
            Ok(value) => value,
            Err(err) => return receipt.fail("validate_existing", err),
        };
        let dirty_status_digest =
            (!dirty.is_empty()).then(|| format!("sha256:{:x}", Sha256::digest(&dirty)));
        receipt.repository = Some(RunRepository {
            root: repo.display().to_string(),
            base_revision: base.clone(),
            branch: None,
            worktree_path: checkout.display().to_string(),
            dirty_status_digest: dirty_status_digest.clone(),
        });
        receipt.stage(
            "validate_checkout_state",
            if dirty_status_digest.is_some() { StageOutcome::Unverified } else { StageOutcome::Verified },
            dirty_status_digest.as_ref().map_or_else(
                || "existing checkout is clean".into(),
                |digest| format!("existing checkout is dirty; bounded status digest {digest}; base alone is not reproducible"),
            ),
        );
        if let Some(collection_id) = &args.collection {
            let response = match super::send_request(&Request {
                id: format!("cli:{}:collection", receipt.run_id),
                method: Method::CollectionCreateMember(CollectionCreateMemberParams {
                    collection_id: collection_id.clone(),
                    cwd: Some(checkout.display().to_string()),
                    env: Default::default(),
                    delegation_parent_id: None,
                    purpose: Some(format!("herdr run {}", receipt.run_id)),
                }),
            }) {
                Ok(value) => value,
                Err(err) => {
                    return finish_failure_with_cleanup(
                        args,
                        receipt,
                        "create_resource",
                        format!("ambiguous collection member create transport failure: {err}"),
                    )
                }
            };
            if let Some(err) = response_error(&response) {
                return finish_failure_with_cleanup(args, receipt, "create_resource", err);
            }
            receipt.created_resources.push(format!(
                "pane:{}",
                response["result"]["created"]["pane"]["pane_id"]
                    .as_str()
                    .unwrap_or("unknown")
            ));
            receipt.stage(
                "create_resource",
                StageOutcome::Created,
                format!("collection member in {collection_id}"),
            );
            collection_launch_from_response(&response)
        } else {
            let response = match super::send_request(&Request {
                id: format!("cli:{}:worktree-open", receipt.run_id),
                method: Method::WorktreeOpen(WorktreeOpenParams {
                    cwd: Some(repo.display().to_string()),
                    path: Some(checkout.display().to_string()),
                    branch: None,
                    label: args.label.clone(),
                    focus: false,
                    repository_id: None,
                    workspace_id: None,
                    trust_repository: false,
                }),
            }) {
                Ok(value) => value,
                Err(err) => {
                    return finish_failure_with_cleanup(
                        args,
                        receipt,
                        "validate_existing",
                        format!("ambiguous worktree open transport failure: {err}"),
                    )
                }
            };
            if let Some(err) = response_error(&response) {
                return finish_failure_with_cleanup(args, receipt, "validate_existing", err);
            }
            let reused = response["result"]["already_open"]
                .as_bool()
                .unwrap_or(false);
            receipt.stage(
                "validate_existing",
                if reused {
                    StageOutcome::Reused
                } else {
                    StageOutcome::Created
                },
                checkout.display().to_string(),
            );
            if !reused {
                receipt.created_resources.push(format!(
                    "workspace:{}",
                    response["result"]["workspace"]["id"]
                        .as_str()
                        .unwrap_or("unknown")
                ));
            }
            launch_from_response(&response)
        }
    };
    let (workspace_id, pane_id) = match launch {
        Ok(value) => value,
        Err(err) => return finish_failure_with_cleanup(args, receipt, "create_resource", err),
    };
    receipt.workspace_id = Some(workspace_id.clone());
    receipt.pane_id = Some(pane_id.clone());

    let model_arg = match &args.provider {
        Some(provider) => format!(
            "{provider}/{}",
            args.model.as_deref().expect("validated args")
        ),
        None => args.model.clone().expect("validated args"),
    };
    let start = match start_agent_subprocess(args, &pane_id, &model_arg) {
        Ok(value) => value,
        Err(err) => return finish_failure_with_cleanup(args, receipt, "start_agent", err),
    };
    let agent: AgentInfo = match serde_json::from_value(start["result"]["agent"].clone()) {
        Ok(value) => value,
        Err(err) => {
            return finish_failure_with_cleanup(
                args,
                receipt,
                "start_agent",
                format!("invalid agent start response: {err}"),
            )
        }
    };
    if agent.workspace_id != workspace_id || agent.pane_id != pane_id || !agent.interactive_ready {
        return finish_failure_with_cleanup(
            args,
            receipt,
            "start_agent",
            "agent start returned the wrong pane or was not interactively ready".into(),
        );
    }
    let Some(session) = agent.agent_session.clone() else {
        return finish_failure_with_cleanup(
            args,
            receipt,
            "start_agent",
            "agent start did not establish a canonical agent_session".into(),
        );
    };
    receipt.terminal_id = Some(agent.terminal_id.clone());
    receipt.agent_session = Some(session.clone());
    receipt.stage(
        "start_agent",
        StageOutcome::Verified,
        format!("{} on {}", args.name.as_deref().unwrap_or("agent"), pane_id),
    );
    let child_identity = identity_from_agent(&agent, session);
    if current_identity(&receipt.run_id, &pane_id).as_ref() != Ok(&child_identity) {
        return finish_failure_with_cleanup(
            args,
            receipt,
            "apply_profile",
            "child identity changed before profile application".into(),
        );
    }

    match apply_profile(
        &profile_helper,
        &child_identity,
        args.role.as_deref().expect("validated args"),
        Duration::from_secs(10),
    ) {
        Ok(detail) => receipt.stage("apply_profile", StageOutcome::Verified, detail),
        Err(err) => return finish_failure_with_cleanup(args, receipt, "apply_profile", err),
    }
    let post_profile_agent = match current_agent(&receipt.run_id, &pane_id) {
        Ok(value) if identity_from_agent_info(&value).as_ref() == Some(&child_identity) => value,
        Ok(_) => {
            return finish_failure_with_cleanup(
                args,
                receipt,
                "apply_profile",
                "child identity changed during profile application".into(),
            )
        }
        Err(err) => return finish_failure_with_cleanup(args, receipt, "apply_profile", err),
    };

    let model_verified = post_profile_agent
        .tokens
        .get("model")
        .is_some_and(|value| value == &model_arg);
    let thinking_verified = post_profile_agent
        .tokens
        .get("thinking")
        .or_else(|| post_profile_agent.tokens.get("effort"))
        .is_some_and(|value| {
            value.eq_ignore_ascii_case(args.thinking.as_deref().expect("validated args"))
        });
    receipt.stage(
        "verify_model_thinking",
        if model_verified && thinking_verified {
            StageOutcome::Verified
        } else {
            StageOutcome::Unverified
        },
        format!(
            "requested model={model_arg} thinking={}; authoritative metadata model={} thinking={}",
            args.thinking.as_deref().unwrap(),
            model_verified,
            thinking_verified
        ),
    );

    let envelope = HerdrHandoff {
        version: 1,
        message_id: format!("{}:assignment", receipt.run_id),
        created_at: receipt.created_at.clone(),
        sender: parent,
        recipient: child_identity,
        kind: HandoffKind::Assignment,
        correlation_id: Some(receipt.run_id.clone()),
        reply_to_id: None,
        task: None,
        summary: assignment,
        artifact_refs: vec![ArtifactRef {
            kind: crate::api::schema::ArtifactKind::Receipt,
            value: format!("run:{}", receipt.run_id),
            digest: receipt.assignment_digest.clone(),
        }],
    };
    let response = match super::send_request(&Request {
        id: format!("cli:{}:assignment", receipt.run_id),
        method: Method::HandoffSend(HandoffSendParams { envelope }),
    }) {
        Ok(value) => value,
        Err(err) => {
            return finish_failure_with_cleanup(args, receipt, "submit_assignment", err.to_string())
        }
    };
    if let Some(err) = response_error(&response) {
        return finish_failure_with_cleanup(args, receipt, "submit_assignment", err);
    }
    let outcome = serde_json::from_value::<HandoffTransportOutcome>(
        response["result"]["receipt"]["outcome"].clone(),
    );
    if !matches!(
        outcome,
        Ok(HandoffTransportOutcome::RuntimeTransactionAdmitted)
    ) {
        return finish_failure_with_cleanup(
            args,
            receipt,
            "submit_assignment",
            format!(
                "handoff was not admitted: {}",
                response["result"]["receipt"]["outcome"]
            ),
        );
    }
    receipt.stage(
        "submit_assignment",
        StageOutcome::Verified,
        "runtime transaction admitted; Pi/gate/agent acknowledgement remains unknown",
    );
    0
}

fn finish_failure_with_cleanup(
    args: &RunArgs,
    receipt: &mut RunReceipt,
    stage: &'static str,
    error: String,
) -> i32 {
    let code = receipt.fail(stage, error);
    if args.cleanup_on_failure {
        safe_cleanup(receipt);
    } else {
        receipt.stage(
            "cleanup",
            StageOutcome::Skipped,
            "rollback disabled by default",
        );
    }
    code
}

fn safe_cleanup(receipt: &mut RunReceipt) {
    // Cleanup is deliberately conservative: a started agent or collection member may already have
    // produced work. Herdr leaves it visible rather than infer that destructive removal is safe.
    receipt.stage("cleanup", StageOutcome::Skipped, "identity-safe cleanup could not prove the created resource remained untouched; resources retained");
}

fn start_agent_subprocess(
    args: &RunArgs,
    pane_id: &str,
    model: &str,
) -> Result<serde_json::Value, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let output = Command::new(exe)
        .args([
            "agent",
            "start",
            args.name.as_deref().unwrap(),
            "--kind",
            "pi",
            "--pane",
            pane_id,
            "--timeout",
            &args.timeout_ms.to_string(),
            "--",
            "--model",
            model,
            "--thinking",
            args.thinking.as_deref().unwrap(),
        ])
        .output()
        .map_err(|err| format!("cannot start agent command: {err}"))?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "agent start returned invalid JSON: {err}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    if !output.status.success() || value.get("error").is_some() {
        return Err(response_error(&value)
            .unwrap_or_else(|| format!("agent start exited {}", output.status)));
    }
    Ok(value)
}

fn apply_profile(
    helper: &Path,
    identity: &CanonicalHerdrIdentity,
    profile: &str,
    timeout: Duration,
) -> Result<String, String> {
    let session_kind = serde_json::to_value(identity.agent_session.kind)
        .expect("session kind serializes")
        .as_str()
        .expect("session kind serializes as a string")
        .to_string();
    let mut child = Command::new(helper)
        .args([
            "use",
            "--workspace",
            &identity.workspace_id,
            "--pane",
            &identity.pane_id,
            "--terminal",
            &identity.terminal_id,
            "--agent",
            &identity.agent_session.agent,
            "--session-source",
            &identity.agent_session.source,
            "--session-kind",
            &session_kind,
            "--session-value",
            &identity.agent_session.value,
            "--profile",
            profile,
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("profile helper unavailable: {err}"))?;
    if child
        .wait_timeout(timeout)
        .map_err(|err| format!("cannot wait for profile helper: {err}"))?
        .is_none()
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "profile helper exceeded {} ms and was terminated",
            timeout.as_millis()
        ));
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("cannot collect profile helper output: {err}"))?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("profile helper returned invalid JSON: {err}"))?;
    let receipt_session =
        serde_json::from_value::<AgentSessionInfo>(value["agentSession"].clone()).ok();
    if !output.status.success()
        || value["version"].as_u64() != Some(1)
        || value["applied"].as_bool() != Some(true)
        || value["verified"].as_bool() != Some(true)
        || value["workspaceId"].as_str() != Some(identity.workspace_id.as_str())
        || value["paneId"].as_str() != Some(identity.pane_id.as_str())
        || value["terminalId"].as_str() != Some(identity.terminal_id.as_str())
        || receipt_session.as_ref() != Some(&identity.agent_session)
        || value["profile"].as_str() != Some(profile)
    {
        return Err(format!(
            "profile helper did not return an exact-session applied+verified v1 receipt: {value}"
        ));
    }
    Ok(format!(
        "profile {profile} helper-attested as applied and read back for the exact child session"
    ))
}

fn current_agent(run_id: &str, pane_id: &str) -> Result<AgentInfo, String> {
    let response = super::send_request(&Request {
        id: format!("cli:{run_id}:identity"),
        method: Method::AgentGet(AgentTarget {
            target: pane_id.into(),
        }),
    })
    .map_err(|err| err.to_string())?;
    if let Some(err) = response_error(&response) {
        return Err(err);
    }
    serde_json::from_value(response["result"]["agent"].clone())
        .map_err(|err| format!("invalid identity lookup response: {err}"))
}

fn current_identity(run_id: &str, pane_id: &str) -> Result<CanonicalHerdrIdentity, String> {
    let agent = current_agent(run_id, pane_id)?;
    identity_from_agent_info(&agent).ok_or_else(|| "agent has no canonical session identity".into())
}

fn identity_from_agent_info(agent: &AgentInfo) -> Option<CanonicalHerdrIdentity> {
    Some(identity_from_agent(agent, agent.agent_session.clone()?))
}

fn identity_from_agent(agent: &AgentInfo, session: AgentSessionInfo) -> CanonicalHerdrIdentity {
    CanonicalHerdrIdentity {
        workspace_id: agent.workspace_id.clone(),
        pane_id: agent.pane_id.clone(),
        terminal_id: agent.terminal_id.clone(),
        agent_session: session,
    }
}

fn launch_from_response(value: &serde_json::Value) -> Result<(String, String), String> {
    Ok((
        value["result"]["workspace"]["id"]
            .as_str()
            .ok_or("response missing workspace id")?
            .into(),
        value["result"]["root_pane"]["pane_id"]
            .as_str()
            .ok_or("response missing root pane id")?
            .into(),
    ))
}
fn collection_launch_from_response(value: &serde_json::Value) -> Result<(String, String), String> {
    Ok((
        value["result"]["created"]["pane"]["workspace_id"]
            .as_str()
            .ok_or("response missing workspace id")?
            .into(),
        value["result"]["created"]["pane"]["pane_id"]
            .as_str()
            .ok_or("response missing pane id")?
            .into(),
    ))
}
fn response_error(value: &serde_json::Value) -> Option<String> {
    value.get("error").map(|error| {
        format!(
            "{}: {}",
            error["code"].as_str().unwrap_or("error"),
            error["message"].as_str().unwrap_or("request failed")
        )
    })
}

fn canonical_repo(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|err| format!("cannot resolve repository {}: {err}", path.display()))?;
    let root = git_output(&canonical, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root)
        .canonicalize()
        .map_err(|err| err.to_string())?;
    if root != canonical {
        return Err(format!(
            "repository path must be exact root {}; got {}",
            root.display(),
            canonical.display()
        ));
    }
    Ok(root)
}
fn canonical_existing_checkout(repo: &Path, checkout: &Path) -> Result<PathBuf, String> {
    let checkout = checkout
        .canonicalize()
        .map_err(|err| format!("cannot resolve checkout: {err}"))?;
    let checkout_root = PathBuf::from(git_output(&checkout, &["rev-parse", "--show-toplevel"])?)
        .canonicalize()
        .map_err(|err| format!("cannot resolve checkout root: {err}"))?;
    if checkout != checkout_root {
        return Err(format!(
            "existing checkout path must be exact root {}; got {}",
            checkout_root.display(),
            checkout.display()
        ));
    }
    let common = git_output(&checkout, &["rev-parse", "--git-common-dir"])?;
    let repo_common = git_output(repo, &["rev-parse", "--git-common-dir"])?;
    let normalize = |base: &Path, value: String| {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            base.join(path)
        }
        .canonicalize()
    };
    if normalize(&checkout, common).map_err(|err| err.to_string())?
        != normalize(repo, repo_common).map_err(|err| err.to_string())?
    {
        return Err("existing checkout belongs to a different repository".into());
    }
    Ok(checkout)
}
fn conventional_worktree_path(repo: &Path, branch: &str) -> Result<PathBuf, String> {
    if branch.is_empty()
        || branch.starts_with('/')
        || branch.contains('\\')
        || branch
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("branch is unsafe for the worktree path convention".into());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let repo_name = repo.file_name().ok_or("repository has no name")?;
    let expected_repo = home.join("Projects").join(repo_name);
    if repo
        != expected_repo
            .canonicalize()
            .map_err(|err| format!("primary repository convention path is unavailable: {err}"))?
    {
        return Err(format!(
            "new branches require primary checkout at {}",
            expected_repo.display()
        ));
    }
    Ok(home
        .join("Projects/.worktrees")
        .join(repo_name)
        .join(branch))
}
fn git_output_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output_bytes(cwd, args)?;
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}
fn read_json_arg<T: serde::de::DeserializeOwned>(source: &str) -> Result<T, String> {
    let text = if Path::new(source).is_file() {
        std::fs::read_to_string(source).map_err(|err| err.to_string())?
    } else {
        source.into()
    };
    serde_json::from_str(&text).map_err(|err| err.to_string())
}
fn forbidden_assignment_char(ch: char) -> bool {
    ch != '\n'
        && (ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_args(args: &[String]) -> Result<RunArgs, String> {
    let mut out = RunArgs {
        timeout_ms: 30_000,
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let name = args[i].as_str();
        if name == "--cleanup-on-failure" {
            out.cleanup_on_failure = true;
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing value for {name}"))?
            .clone();
        match name {
            "--repo" => out.repo = Some(value.into()),
            "--base" => out.base = Some(value),
            "--branch" => out.branch = Some(value),
            "--existing" => out.existing = Some(value.into()),
            "--label" => out.label = Some(value),
            "--role" => out.role = Some(value),
            "--model" => out.model = Some(value),
            "--provider" => out.provider = Some(value),
            "--thinking" => out.thinking = Some(value),
            "--complexity-reason" => out.complexity_reason = Some(value),
            "--assignment-file" => out.assignment_file = Some(value.into()),
            "--parent" => out.parent = Some(value),
            "--name" => out.name = Some(value),
            "--collection" => out.collection = Some(value),
            "--profile-helper" => out.profile_helper = Some(value.into()),
            "--timeout" => out.timeout_ms = value.parse().map_err(|_| "invalid --timeout")?,
            _ => return Err(format!("unknown option: {name}")),
        }
        i += 2;
    }
    for (name, missing) in [
        ("--repo", out.repo.is_none()),
        ("--base", out.base.is_none()),
        ("--role", out.role.is_none()),
        ("--model", out.model.is_none()),
        ("--thinking", out.thinking.is_none()),
        ("--assignment-file", out.assignment_file.is_none()),
        ("--parent", out.parent.is_none()),
        ("--name", out.name.is_none()),
    ] {
        if missing {
            return Err(format!("missing required {name}"));
        }
    }
    let role = out.role.as_deref().expect("required role checked");
    if !role.ends_with(".md")
        || role.contains('/')
        || role.contains('\\')
        || role.contains("..")
        || role.len() > 128
        || role.chars().any(char::is_control)
    {
        return Err("--role must be a safe profile filename ending in .md".into());
    }
    if out.branch.is_some() == out.existing.is_some() {
        return Err("exactly one of --branch (new-only mode) or --existing is required".into());
    }
    if out.collection.is_some() && out.existing.is_none() {
        return Err("--collection requires --existing and is mutually exclusive with new-worktree workspace mode".into());
    }
    if !matches!(out.thinking.as_deref(), Some("low" | "medium" | "high")) {
        return Err("--thinking must be low, medium, or high".into());
    }
    if matches!(out.thinking.as_deref(), Some("medium" | "high"))
        && out.complexity_reason.as_deref().is_none_or(str::is_empty)
    {
        return Err("medium/high thinking requires --complexity-reason".into());
    }
    if out.timeout_ms <= 3_000 || out.timeout_ms > 300_000 {
        return Err("--timeout must be greater than 3000 and at most 300000".into());
    }
    Ok(out)
}

fn print_help() {
    eprintln!("usage: herdr run --repo PATH --base REF (--branch NAME | --existing PATH) --role PROFILE.md --model MODEL [--provider PROVIDER] --thinking low|medium|high [--complexity-reason TEXT] --assignment-file PATH --parent JSON|PATH --name NAME [--label TEXT] [--collection ID] [--profile-helper PATH] [--timeout MS] [--cleanup-on-failure]");
    eprintln!("collection launch mode is workspace-local and requires --existing; it cannot be combined with --branch");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Vec<String> {
        "--repo /tmp/r --base HEAD --existing /tmp/r --role owner.md --model m --thinking low --assignment-file /tmp/a --parent {} --name child".split_whitespace().map(str::to_string).collect()
    }
    #[test]
    fn modes_are_exclusive() {
        let mut args = base();
        args.extend(["--branch".into(), "x".into()]);
        assert!(parse_args(&args).is_err());
    }
    #[test]
    fn collection_requires_existing() {
        let mut args = base();
        args.splice(4..6, ["--branch".into(), "x".into()]);
        args.extend(["--collection".into(), "c1".into()]);
        assert!(parse_args(&args).is_err());
    }
    #[test]
    fn high_thinking_requires_reason() {
        let mut args = base();
        let pos = args.iter().position(|v| v == "low").unwrap();
        args[pos] = "high".into();
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn worktree_convention_rejects_windows_separator_escape() {
        assert!(
            conventional_worktree_path(Path::new("/tmp/repository"), "feature\\outside").is_err()
        );
    }

    #[test]
    fn existing_checkout_requires_the_checkout_root() {
        let _env = crate::test_env::shared();
        let root = std::env::temp_dir().join(format!(
            "herdr-run-checkout-root-{}-{}",
            std::process::id(),
            unix_seconds()
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "--quiet"]);
        assert_eq!(canonical_existing_checkout(&root, &root).unwrap(), root);
        assert!(canonical_existing_checkout(&root, &root.join("nested"))
            .unwrap_err()
            .contains("must be exact root"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn receipt_exposes_the_pinned_terminal_identity() {
        let args = RunArgs::default();
        let mut receipt = RunReceipt::new("run-test".into(), &args);
        receipt.terminal_id = Some("terminal-1".into());
        let encoded = serde_json::to_value(receipt).unwrap();
        assert_eq!(encoded["terminalId"], "terminal-1");
    }

    #[test]
    fn ambiguous_start_failure_retains_created_resources() {
        let args = RunArgs {
            cleanup_on_failure: true,
            ..Default::default()
        };
        let mut receipt = RunReceipt::new("run-test".into(), &args);
        receipt.created_resources.push("workspace:w1".into());
        assert_eq!(
            finish_failure_with_cleanup(
                &args,
                &mut receipt,
                "start_agent",
                "timeout after launch injection".into()
            ),
            1
        );
        assert_eq!(receipt.created_resources, ["workspace:w1"]);
        assert!(matches!(
            receipt.stages.last().unwrap().outcome,
            StageOutcome::Skipped
        ));
        assert!(receipt.stages.last().unwrap().detail.contains("retained"));
    }

    #[cfg(unix)]
    #[test]
    fn profile_helper_requires_exact_verified_receipt() {
        let _env = crate::test_env::shared();
        let root =
            std::env::temp_dir().join(format!("herdr-profile-helper-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let helper = root.join("helper");
        crate::test_env::write_executable(&helper, "#!/bin/sh\nprintf '%s\\n' '{\"version\":1,\"applied\":true,\"verified\":true,\"workspaceId\":\"w1\",\"paneId\":\"w1:p1\",\"terminalId\":\"term1\",\"agentSession\":{\"source\":\"herdr:pi\",\"agent\":\"pi\",\"kind\":\"id\",\"value\":\"session1\"},\"profile\":\"owner.md\"}'\n");
        let identity = CanonicalHerdrIdentity {
            workspace_id: "w1".into(),
            pane_id: "w1:p1".into(),
            terminal_id: "term1".into(),
            agent_session: AgentSessionInfo {
                source: "herdr:pi".into(),
                agent: "pi".into(),
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: "session1".into(),
            },
        };
        assert!(apply_profile(&helper, &identity, "owner.md", Duration::from_secs(1)).is_ok());
        let mut replacement = identity.clone();
        replacement.pane_id = "w1:p2".into();
        assert!(apply_profile(&helper, &replacement, "owner.md", Duration::from_secs(1)).is_err());
        crate::test_env::write_executable(&helper, "#!/bin/sh\nexec sleep 5\n");
        let started = std::time::Instant::now();
        let error =
            apply_profile(&helper, &identity, "owner.md", Duration::from_millis(25)).unwrap_err();
        assert!(error.contains("terminated"));
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(root).unwrap();
    }
}
