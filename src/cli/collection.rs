use std::{collections::HashMap, path::PathBuf, time::Duration};

use crate::api::schema::*;

pub(super) fn run_collection_command(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return usage(2);
    };
    if command == "helper-launch" {
        return match parse_helper_launch(&args[1..]) {
            Ok(params) => helper_launch(params),
            Err(message) => {
                eprintln!("{message}");
                usage(2)
            }
        };
    }
    let result = match command {
        "list" => parse_list(&args[1..]).map(Method::CollectionList),
        "get" => one(&args[1..])
            .map(|collection_id| Method::CollectionGet(CollectionTarget { collection_id })),
        "create" => parse_create(&args[1..]).map(Method::CollectionCreate),
        "add" => two(&args[1..]).map(|(collection_id, pane_id)| {
            Method::CollectionAdd(CollectionAddParams {
                collection_id,
                pane_id,
            })
        }),
        "move" => two(&args[1..]).map(|(pane_id, collection_id)| {
            Method::CollectionMove(CollectionMoveParams {
                pane_id,
                collection_id,
            })
        }),
        "promote" => parse_promote(&args[1..]).map(Method::CollectionPromote),
        "select" => parse_select(&args[1..]).map(Method::CollectionSelect),
        "reorder" => parse_reorder(&args[1..]).map(Method::CollectionReorder),
        "archive" => two(&args[1..]).map(|(collection_id, pane_id)| {
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id,
                pane_id,
            })
        }),
        "restore" => two(&args[1..]).map(|(collection_id, pane_id)| {
            Method::CollectionRestore(CollectionMemberTarget {
                collection_id,
                pane_id,
            })
        }),
        "member-create" => parse_member_create(&args[1..]).map(Method::CollectionCreateMember),
        "close" => parse_close(&args[1..]).map(Method::CollectionClose),
        "help" | "--help" | "-h" => return usage(0),
        _ => return usage(2),
    };
    match result {
        Ok(method) => super::runtime::collection(method),
        Err(message) => {
            eprintln!("{message}");
            usage(2)
        }
    }
}

fn helper_abort(
    collection_id: &str,
    pane_id: &str,
    terminal_id: &str,
) -> std::io::Result<serde_json::Value> {
    super::send_request(&Request {
        id: "cli:collection:helper-launch:rollback".into(),
        method: Method::CollectionHelperAbort(CollectionHelperAbortParams {
            collection_id: collection_id.into(),
            pane_id: pane_id.into(),
            terminal_id: terminal_id.into(),
        }),
    })
}

fn malformed_helper_launch_response(
    collection_id: &str,
    launched_value: Option<&serde_json::Value>,
    mut abort: impl FnMut(&str, &str, &str) -> std::io::Result<serde_json::Value>,
) -> serde_json::Value {
    let pane_id = launched_value
        .and_then(|value| value.pointer("/created/pane/pane_id"))
        .and_then(serde_json::Value::as_str);
    let terminal_id = launched_value
        .and_then(|value| value.pointer("/agent/terminal_id"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            launched_value
                .and_then(|value| value.pointer("/created/pane/terminal_id"))
                .and_then(serde_json::Value::as_str)
        });
    let (rollback_status, rollback_error) = match (pane_id, terminal_id) {
        (Some(pane_id), Some(terminal_id)) => match abort(collection_id, pane_id, terminal_id) {
            Ok(value) if value.get("error").is_none() => ("completed", None),
            Ok(value) => ("failed", value.get("error").cloned()),
            Err(error) => (
                "uncertain",
                Some(serde_json::json!({ "message": error.to_string() })),
            ),
        },
        _ => ("unavailable", None),
    };
    serde_json::json!({
        "id": "cli:collection:helper-launch",
        "error": {
            "code": "collection_helper_invalid_response",
            "message": "helper-launch response omitted or malformed the created helper identity",
            "rollback_status": rollback_status,
            "rollback_error": rollback_error,
            "collection_id": collection_id,
            "pane_id": pane_id,
            "terminal_id": terminal_id,
        }
    })
}

#[derive(Debug)]
struct ParsedHelperLaunch {
    params: CollectionHelperLaunchParams,
    assignment: String,
}

fn read_helper_assignment(path: &str) -> Result<String, String> {
    const MAX_ASSIGNMENT_BYTES: u64 = 16 * 1024;
    let path = PathBuf::from(path);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|err| format!("failed to inspect --assignment-file {path:?}: {err}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("--assignment-file must be a regular non-symlink file".into());
    }
    if metadata.len() > MAX_ASSIGNMENT_BYTES {
        return Err(format!(
            "--assignment-file exceeds {MAX_ASSIGNMENT_BYTES} bytes"
        ));
    }
    let assignment = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read --assignment-file {path:?}: {err}"))?;
    if assignment.trim().is_empty() || assignment.as_bytes().contains(&0) {
        return Err("--assignment-file must contain nonempty UTF-8 text without NUL".into());
    }
    Ok(assignment)
}

fn helper_launch(parsed: ParsedHelperLaunch) -> std::io::Result<i32> {
    let ParsedHelperLaunch { params, assignment } = parsed;
    let expected_kind = crate::detect::parse_agent_label(&params.kind)
        .map(crate::detect::agent_label)
        .unwrap_or(params.kind.as_str())
        .to_string();
    let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(30_000));
    let collection_id = params.collection_id.clone();
    let name = params.name.clone();
    let mut response = super::send_request(&Request {
        id: "cli:collection:helper-launch".into(),
        method: Method::CollectionHelperLaunch(params),
    })?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    let launched_value = response
        .get("result")
        .and_then(|result| result.get("launched"))
        .cloned();
    let launched = launched_value
        .clone()
        .and_then(|value| serde_json::from_value::<CollectionHelperLaunchResult>(value).ok());
    let Some(launched) = launched else {
        let malformed =
            malformed_helper_launch_response(&collection_id, launched_value.as_ref(), helper_abort);
        return super::print_response(&malformed);
    };
    let pane_id = launched.created.pane.pane_id;
    let terminal_id = launched.agent.terminal_id;
    match super::agent::wait_for_named_agent(
        &name,
        &pane_id,
        timeout,
        &expected_kind,
        &terminal_id,
        true,
    ) {
        Ok(Ok(agent)) => {
            let session = agent.get("agent_session").cloned();
            let prompt = super::send_request(&Request {
                id: "cli:collection:helper-launch:assignment".into(),
                method: Method::AgentPrompt(AgentPromptParams {
                    target: pane_id.clone(),
                    text: assignment,
                    wait: None,
                    send: Default::default(),
                }),
            });
            let prompt_error = match prompt {
                Ok(value) if value.get("error").is_some() => value.get("error").cloned(),
                Ok(value) => {
                    let prompted = value.pointer("/result/agent");
                    match prompted {
                        Some(prompted)
                            if prompted.get("pane_id").and_then(serde_json::Value::as_str)
                                == Some(pane_id.as_str())
                                && prompted
                                    .get("terminal_id")
                                    .and_then(serde_json::Value::as_str)
                                    == Some(terminal_id.as_str())
                                && prompted.get("agent_session").cloned() == session =>
                        {
                            None
                        }
                        _ => Some(serde_json::json!({
                            "code": "collection_helper_assignment_identity_mismatch",
                            "message": "assignment transport did not return the exact created helper identity"
                        })),
                    }
                }
                Err(err) => Some(serde_json::json!({
                    "code": "collection_helper_assignment_transport_failed",
                    "message": err.to_string()
                })),
            };
            if let Some(assignment_error) = prompt_error {
                let rollback = helper_abort(&collection_id, &pane_id, &terminal_id);
                let (rollback_status, rollback_error) = match rollback {
                    Ok(value) if value.get("error").is_none() => ("completed", None),
                    Ok(value) => ("failed", value.get("error").cloned()),
                    Err(err) => (
                        "uncertain",
                        Some(serde_json::json!({"message": err.to_string()})),
                    ),
                };
                return super::print_response(&serde_json::json!({
                    "id": "cli:collection:helper-launch",
                    "error": {
                        "code": "collection_helper_assignment_failed",
                        "message": "helper became ready but exact assignment transport failed",
                        "assignment_error": assignment_error,
                        "rollback_status": rollback_status,
                        "rollback_error": rollback_error,
                        "collection_id": collection_id,
                        "pane_id": pane_id,
                        "terminal_id": terminal_id,
                    }
                }));
            }
            response["result"]["launched"]["agent"] = agent;
            response["result"]["assignment_transport"] = serde_json::json!({
                "outcome": "runtime_transaction_admitted",
                "target_pane_id": pane_id,
                "target_terminal_id": terminal_id,
                "gate_admission": "unknown",
                "model_execution": "unknown",
                "todo_acceptance": "unknown"
            });
            super::print_response(&response)
        }
        Ok(Err(start_error)) => match helper_abort(&collection_id, &pane_id, &terminal_id) {
            Ok(rollback) if rollback.get("error").is_none() => super::print_response(&start_error),
            Ok(rollback) => {
                let combined = serde_json::json!({
                    "id": "cli:collection:helper-launch",
                    "error": {
                        "code": "collection_helper_rollback_failed",
                        "message": "helper startup failed and the created collection member could not be removed",
                        "startup_error": start_error.get("error"),
                        "rollback_error": rollback.get("error"),
                        "collection_id": collection_id,
                        "pane_id": pane_id,
                        "terminal_id": terminal_id,
                    }
                });
                super::print_response(&combined)
            }
            Err(rollback_error) => {
                let combined = serde_json::json!({
                    "id": "cli:collection:helper-launch",
                    "error": {
                        "code": "collection_helper_rollback_uncertain",
                        "message": "helper startup failed and rollback could not be confirmed",
                        "startup_error": start_error.get("error"),
                        "rollback_error": rollback_error.to_string(),
                        "collection_id": collection_id,
                        "pane_id": pane_id,
                        "terminal_id": terminal_id,
                    }
                });
                super::print_response(&combined)
            }
        },
        Err(err) => match helper_abort(&collection_id, &pane_id, &terminal_id) {
            Ok(rollback) if rollback.get("error").is_none() => Err(err),
            Ok(rollback) => Err(std::io::Error::new(
                err.kind(),
                format!(
                    "{err}; helper rollback failed for {pane_id}/{terminal_id}: {}",
                    rollback["error"]
                ),
            )),
            Err(rollback_err) => Err(std::io::Error::new(
                err.kind(),
                format!(
                    "{err}; helper rollback outcome is uncertain for {pane_id}/{terminal_id}: {rollback_err}"
                ),
            )),
        },
    }
}

fn parse_list(args: &[String]) -> Result<CollectionListParams, String> {
    let mut workspace_id = None;
    let mut tab_id = None;
    parse_options(args, |name, value| match name {
        "--workspace" => {
            workspace_id = Some(required_value(name, value)?);
            Ok(())
        }
        "--tab" => {
            tab_id = Some(required_value(name, value)?);
            Ok(())
        }
        _ => Err(format!("unknown option: {name}")),
    })?;
    Ok(CollectionListParams {
        workspace_id,
        tab_id,
    })
}
fn parse_create(args: &[String]) -> Result<CollectionCreateParams, String> {
    let mut target = None;
    let mut direction = None;
    let mut ratio = None;
    let mut label = None;
    let mut focus = false;
    parse_options(args, |name, value| match name {
        "--target-pane" => {
            target = Some(required_value(name, value)?);
            Ok(())
        }
        "--direction" => {
            direction = Some(parse_direction(&required_value(name, value)?)?);
            Ok(())
        }
        "--ratio" => {
            ratio = Some(parse_ratio(&required_value(name, value)?)?);
            Ok(())
        }
        "--label" => {
            label = Some(required_value(name, value)?);
            Ok(())
        }
        "--focus" => {
            focus = true;
            Ok(())
        }
        "--no-focus" => {
            focus = false;
            Ok(())
        }
        _ => Err(format!("unknown option: {name}")),
    })?;
    Ok(CollectionCreateParams {
        target_pane_id: target.ok_or("missing --target-pane")?,
        direction: direction.ok_or("missing --direction")?,
        ratio,
        label,
        focus,
    })
}
fn parse_promote(args: &[String]) -> Result<CollectionPromoteParams, String> {
    let pane_id = args
        .first()
        .filter(|v| !v.starts_with('-'))
        .cloned()
        .ok_or("missing pane_id")?;
    let mut target = None;
    let mut direction = None;
    let mut ratio = None;
    let mut focus = false;
    parse_options(&args[1..], |name, value| match name {
        "--target-pane" => {
            target = Some(required_value(name, value)?);
            Ok(())
        }
        "--direction" => {
            direction = Some(parse_direction(&required_value(name, value)?)?);
            Ok(())
        }
        "--ratio" => {
            ratio = Some(parse_ratio(&required_value(name, value)?)?);
            Ok(())
        }
        "--focus" => {
            focus = true;
            Ok(())
        }
        "--no-focus" => {
            focus = false;
            Ok(())
        }
        _ => Err(format!("unknown option: {name}")),
    })?;
    Ok(CollectionPromoteParams {
        pane_id,
        target_pane_id: target.ok_or("missing --target-pane")?,
        direction: direction.ok_or("missing --direction")?,
        ratio,
        focus,
    })
}
fn parse_select(args: &[String]) -> Result<CollectionSelectParams, String> {
    if args.len() < 2 {
        return Err("expected collection_id and pane_id".into());
    }
    let mut focus = false;
    for arg in &args[2..] {
        match arg.as_str() {
            "--focus" => focus = true,
            "--no-focus" => focus = false,
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    Ok(CollectionSelectParams {
        collection_id: args[0].clone(),
        pane_id: args[1].clone(),
        focus,
    })
}
fn parse_reorder(args: &[String]) -> Result<CollectionReorderParams, String> {
    if args.len() != 4 || args[2] != "--index" {
        return Err("usage: herdr collection reorder <collection_id> <pane_id> --index N".into());
    }
    Ok(CollectionReorderParams {
        collection_id: args[0].clone(),
        pane_id: args[1].clone(),
        index: args[3].parse().map_err(|_| "invalid index")?,
    })
}
fn parse_close(args: &[String]) -> Result<CollectionCloseParams, String> {
    let collection_id = args
        .first()
        .filter(|v| !v.starts_with('-'))
        .cloned()
        .ok_or("missing collection_id")?;
    let mut disposition = None;
    let mut focus_promoted = false;
    parse_options(&args[1..], |name, _value| match name {
        "--cascade-close" => {
            if disposition
                .replace(CollectionCloseDisposition::CascadeClose)
                .is_some()
            {
                return Err("choose one disposition".into());
            }
            Ok(())
        }
        "--promote-members" => {
            if disposition
                .replace(CollectionCloseDisposition::PromoteMembers)
                .is_some()
            {
                return Err("choose one disposition".into());
            }
            Ok(())
        }
        "--focus-promoted" => {
            focus_promoted = true;
            Ok(())
        }
        _ => Err(format!("unknown option: {name}")),
    })?;
    Ok(CollectionCloseParams {
        collection_id,
        disposition,
        target_pane_id: None,
        focus_promoted,
    })
}

fn parse_member_create(args: &[String]) -> Result<CollectionCreateMemberParams, String> {
    let collection_id = args
        .first()
        .filter(|v| !v.starts_with('-'))
        .cloned()
        .ok_or("missing collection_id")?;
    let mut cwd = None;
    let mut env = HashMap::new();
    let mut delegation_parent_id = None;
    let mut purpose = None;
    parse_options(&args[1..], |name, value| match name {
        "--cwd" => {
            cwd = Some(required_value(name, value)?);
            Ok(())
        }
        "--env" => {
            let raw = required_value(name, value)?;
            let parsed = super::parse_env_assignment(raw.as_str())?;
            env.insert(parsed.0, parsed.1);
            Ok(())
        }
        "--parent" => {
            delegation_parent_id = Some(required_value(name, value)?);
            Ok(())
        }
        "--purpose" => {
            purpose = Some(required_value(name, value)?);
            Ok(())
        }
        _ => Err(format!("unknown option: {name}")),
    })?;
    Ok(CollectionCreateMemberParams {
        collection_id,
        cwd,
        env,
        delegation_parent_id,
        purpose,
    })
}

fn parse_helper_launch(args: &[String]) -> Result<ParsedHelperLaunch, String> {
    let collection_id = args
        .first()
        .filter(|v| !v.starts_with('-'))
        .cloned()
        .ok_or("missing collection_id")?;
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut cwd = None;
    let mut env = HashMap::new();
    let mut delegation_parent_id = None;
    let mut purpose = None;
    let mut name = None;
    let mut kind = None;
    let mut timeout_ms = None;
    let mut assignment_file = None;
    let mut index = 1;
    while index < separator {
        let option = args[index].as_str();
        let value = args
            .get(index + 1)
            .filter(|_| index + 1 < separator)
            .ok_or_else(|| format!("missing value for {option}"))?;
        match option {
            "--cwd" => cwd = Some(value.clone()),
            "--env" => {
                let (key, item) = super::parse_env_assignment(value)?;
                env.insert(key, item);
            }
            "--parent" => delegation_parent_id = Some(value.clone()),
            "--purpose" => purpose = Some(value.clone()),
            "--name" => name = Some(value.clone()),
            "--kind" => kind = Some(value.clone()),
            "--timeout" => timeout_ms = Some(value.parse().map_err(|_| "invalid timeout")?),
            "--assignment-file" => assignment_file = Some(value.clone()),
            _ => return Err(format!("unknown option: {option}")),
        }
        index += 2;
    }
    let assignment_path = assignment_file.ok_or("missing --assignment-file")?;
    let assignment = read_helper_assignment(&assignment_path)?;
    Ok(ParsedHelperLaunch {
        params: CollectionHelperLaunchParams {
            collection_id,
            cwd,
            env,
            delegation_parent_id,
            purpose,
            name: name.ok_or("missing --name")?,
            kind: kind.ok_or("missing --kind")?,
            args: if separator < args.len() {
                args[separator + 1..].to_vec()
            } else {
                Vec::new()
            },
            timeout_ms,
        },
        assignment,
    })
}

fn parse_options(
    args: &[String],
    mut handle: impl FnMut(&str, Option<&String>) -> Result<(), String>,
) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let name = &args[i];
        if !name.starts_with("--") {
            return Err(format!("unexpected argument: {name}"));
        }
        let value = args.get(i + 1).filter(|value| !value.starts_with("--"));
        handle(name, value)?;
        i += if value.is_some() { 2 } else { 1 };
    }
    Ok(())
}
fn required_value(name: &str, value: Option<&String>) -> Result<String, String> {
    value
        .cloned()
        .ok_or_else(|| format!("missing value for {name}"))
}
fn parse_direction(value: &str) -> Result<SplitDirection, String> {
    match value {
        "right" => Ok(SplitDirection::Right),
        "down" => Ok(SplitDirection::Down),
        _ => Err("direction must be right or down".into()),
    }
}
fn parse_ratio(value: &str) -> Result<f32, String> {
    let value = value.parse::<f32>().map_err(|_| "invalid ratio")?;
    value
        .is_finite()
        .then_some(value)
        .ok_or_else(|| "invalid ratio".into())
}
fn one(args: &[String]) -> Result<String, String> {
    if args.len() == 1 {
        Ok(args[0].clone())
    } else {
        Err("expected one ID".into())
    }
}
fn two(args: &[String]) -> Result<(String, String), String> {
    if args.len() == 2 {
        Ok((args[0].clone(), args[1].clone()))
    } else {
        Err("expected two IDs".into())
    }
}
fn usage(code: i32) -> std::io::Result<i32> {
    eprintln!("herdr collection commands: list, get, create, add, move, promote, select, reorder, archive, restore, member-create, helper-launch, close");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_collection_create_promote_close_and_member_create() {
        let create = parse_create(&strings(&[
            "--target-pane",
            "w1:p1",
            "--direction",
            "right",
            "--ratio",
            "0.4",
            "--label",
            "helpers",
            "--no-focus",
        ]))
        .expect("create");
        assert_eq!(create.target_pane_id, "w1:p1");
        assert_eq!(create.ratio, Some(0.4));
        assert!(!create.focus);

        let promote = parse_promote(&strings(&[
            "w1:p2",
            "--target-pane",
            "w1:p1",
            "--direction",
            "down",
            "--focus",
        ]))
        .expect("promote");
        assert_eq!(promote.pane_id, "w1:p2");
        assert!(promote.focus);

        let close = parse_close(&strings(&[
            "collection_1",
            "--promote-members",
            "--focus-promoted",
        ]))
        .expect("close");
        assert_eq!(
            close.disposition,
            Some(CollectionCloseDisposition::PromoteMembers)
        );
        assert!(close.focus_promoted);
        assert!(parse_close(&strings(&[
            "collection_1",
            "--promote-members",
            "--target-pane",
            "w1:p1",
        ]))
        .is_err());

        let member = parse_member_create(&strings(&[
            "collection_1",
            "--cwd",
            "/tmp",
            "--env",
            "ROLE=review",
            "--parent",
            "d1",
            "--purpose",
            "review",
        ]))
        .expect("member create");
        assert_eq!(member.env["ROLE"], "review");
        assert_eq!(member.delegation_parent_id.as_deref(), Some("d1"));

        let assignment_path = std::env::temp_dir().join(format!(
            "herdr-helper-assignment-{}-{}.txt",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&assignment_path, "Review the exact bounded surface.\n")
            .expect("write assignment");
        let launch = parse_helper_launch(&strings(&[
            "collection_1",
            "--cwd",
            "/tmp",
            "--name",
            "reviewer",
            "--kind",
            "pi",
            "--purpose",
            "review",
            "--assignment-file",
            assignment_path.to_str().expect("UTF-8 assignment path"),
            "--",
            "--thinking",
            "low",
        ]))
        .expect("helper launch");
        std::fs::remove_file(assignment_path).expect("remove assignment");
        assert_eq!(launch.params.name, "reviewer");
        assert_eq!(launch.params.args, vec!["--thinking", "low"]);
        assert_eq!(launch.assignment, "Review the exact bounded surface.\n");
    }

    #[test]
    fn malformed_helper_response_uses_created_pane_identity_for_exact_rollback_status() {
        let launched = serde_json::json!({
            "created": {
                "pane": {
                    "pane_id": "w1:p9",
                    "terminal_id": "term_exact"
                }
            },
            "agent": null
        });

        let mut completed_target = None;
        let completed = malformed_helper_launch_response(
            "collection_7",
            Some(&launched),
            |collection, pane, terminal| {
                completed_target = Some((
                    collection.to_string(),
                    pane.to_string(),
                    terminal.to_string(),
                ));
                Ok(serde_json::json!({"result": {"type": "ok"}}))
            },
        );
        assert_eq!(
            completed_target,
            Some(("collection_7".into(), "w1:p9".into(), "term_exact".into()))
        );
        assert_eq!(completed["error"]["rollback_status"], "completed");
        assert_eq!(completed["error"]["terminal_id"], "term_exact");

        let mut failed_target = None;
        let failed = malformed_helper_launch_response(
            "collection_7",
            Some(&launched),
            |collection, pane, terminal| {
                failed_target = Some((
                    collection.to_string(),
                    pane.to_string(),
                    terminal.to_string(),
                ));
                Ok(serde_json::json!({
                    "error": {"code": "collection_helper_rollback_mismatch", "message": "mismatch"}
                }))
            },
        );
        assert_eq!(failed_target, completed_target);
        assert_eq!(failed["error"]["rollback_status"], "failed");
        assert_eq!(
            failed["error"]["rollback_error"]["code"],
            "collection_helper_rollback_mismatch"
        );

        let mut uncertain_target = None;
        let uncertain = malformed_helper_launch_response(
            "collection_7",
            Some(&launched),
            |collection, pane, terminal| {
                uncertain_target = Some((
                    collection.to_string(),
                    pane.to_string(),
                    terminal.to_string(),
                ));
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "socket timeout",
                ))
            },
        );
        assert_eq!(uncertain_target, completed_target);
        assert_eq!(uncertain["error"]["rollback_status"], "uncertain");
        assert_eq!(
            uncertain["error"]["rollback_error"]["message"],
            "socket timeout"
        );
    }

    #[test]
    fn collection_parsers_reject_ambiguous_or_invalid_mutations() {
        assert!(parse_close(&strings(&[
            "collection_1",
            "--cascade-close",
            "--promote-members"
        ]))
        .is_err());
        assert!(parse_create(&strings(&["--target-pane", "p1", "--direction", "left"])).is_err());
        assert!(parse_reorder(&strings(&["collection_1", "p1", "--index", "nope"])).is_err());
        assert!(parse_select(&strings(&["collection_1"])).is_err());
    }
}
