use crate::api::schema::{
    Method, RepositoryInfo, RepositoryMoveParams, RepositoryRenameParams, RepositoryTarget,
    Request, ResponseResult, SuccessResponse,
};

pub(super) fn run_repository_command(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        help();
        return Ok(2);
    };
    match command {
        "list" => repository_list(&args[1..]),
        "get" => repository_get(&args[1..]),
        "focus" | "close" if args.len() == 2 => {
            let target = RepositoryTarget {
                repository_id: args[1].clone(),
            };
            super::send_ok_request(match command {
                "focus" => Method::RepositoryFocus(target),
                _ => Method::RepositoryClose(target),
            })
        }
        "rename" if args.len() >= 3 => {
            super::send_ok_request(Method::RepositoryRename(RepositoryRenameParams {
                repository_id: args[1].clone(),
                label: args[2..].join(" "),
            }))
        }
        "move" if args.len() == 3 => {
            let insert_index = match args[2].parse::<usize>() {
                Ok(index) => index,
                Err(_) => {
                    eprintln!("insert_index must be a non-negative integer");
                    return Ok(2);
                }
            };
            super::send_ok_request(Method::RepositoryMove(RepositoryMoveParams {
                repository_id: args[1].clone(),
                insert_index,
            }))
        }
        "help" | "--help" | "-h" => {
            help();
            Ok(0)
        }
        _ => {
            help();
            Ok(2)
        }
    }
}

fn repository_list(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr repository list [--json]");
            return Ok(2);
        }
    };
    query_repository(Method::RepositoryList(Default::default()), json, |result| {
        let ResponseResult::RepositoryList { repositories } = result else {
            return Err("unexpected repository.list response");
        };
        Ok(format_repository_list(&repositories))
    })
}

fn repository_get(args: &[String]) -> std::io::Result<i32> {
    let (repository_id, json) = match args {
        [repository_id] => (repository_id, false),
        [repository_id, flag] if flag == "--json" => (repository_id, true),
        _ => {
            eprintln!("usage: herdr repository get <repository_id> [--json]");
            return Ok(2);
        }
    };
    query_repository(
        Method::RepositoryGet(RepositoryTarget {
            repository_id: repository_id.clone(),
        }),
        json,
        |result| {
            let ResponseResult::RepositoryInfo { repository } = result else {
                return Err("unexpected repository.get response");
            };
            Ok(format_repository(&repository))
        },
    )
}

fn query_repository(
    method: Method,
    json: bool,
    format_result: impl FnOnce(ResponseResult) -> Result<String, &'static str>,
) -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: "cli:repository:query".into(),
        method,
    })?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }
    let success: SuccessResponse = serde_json::from_value(response)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let output = format_result(success.result)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
    println!("{output}");
    Ok(0)
}

fn format_repository_list(repositories: &[RepositoryInfo]) -> String {
    if repositories.is_empty() {
        return "No repositories.".into();
    }
    repositories
        .iter()
        .map(|repository| {
            format!(
                "{}\t{}\t{} checkout{}\t{}",
                repository.repository_id,
                repository.label,
                repository.checkout_workspace_ids.len(),
                if repository.checkout_workspace_ids.len() == 1 {
                    ""
                } else {
                    "s"
                },
                agent_status_label(repository.agent_status),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn agent_status_label(status: crate::api::schema::AgentStatus) -> &'static str {
    match status {
        crate::api::schema::AgentStatus::Idle => "idle",
        crate::api::schema::AgentStatus::Working => "working",
        crate::api::schema::AgentStatus::Blocked => "blocked",
        crate::api::schema::AgentStatus::Done => "done",
        crate::api::schema::AgentStatus::Unknown => "unknown",
    }
}

fn format_repository(repository: &RepositoryInfo) -> String {
    format!(
        "id: {}\nlabel: {}\ngit common dir: {}\ncheckouts: {}\nfocused: {}\npanes: {}\nactive agents: {}\nagent status: {}\npreferred base: {}",
        repository.repository_id,
        repository.label,
        repository.git_common_dir,
        repository.checkout_workspace_ids.join(", "),
        repository.focused,
        repository.pane_count,
        repository.active_agent_count,
        agent_status_label(repository.agent_status),
        repository.preferred_base.as_deref().unwrap_or("-"),
    )
}

fn help() {
    eprintln!("herdr repository commands:");
    eprintln!("  herdr repository list [--json]");
    eprintln!("  herdr repository get <repository_id> [--json]");
    eprintln!("  herdr repository focus <repository_id>");
    eprintln!("  herdr repository rename <repository_id> <label>");
    eprintln!("  herdr repository move <repository_id> <insert_index>");
    eprintln!("  herdr repository close <repository_id>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;

    fn repository() -> RepositoryInfo {
        RepositoryInfo {
            repository_id: "r123".into(),
            label: "herdr".into(),
            git_common_dir: "/repo/.git".into(),
            checkout_workspace_ids: vec!["w1".into(), "w2".into()],
            last_focused_workspace_id: Some("w2".into()),
            preferred_base: Some("origin/main".into()),
            focused: true,
            pane_count: 3,
            active_agent_count: 1,
            agent_status: AgentStatus::Working,
            descendant_attention_count: 0,
        }
    }

    #[test]
    fn human_repository_queries_are_non_empty() {
        let repository = repository();
        let list = format_repository_list(std::slice::from_ref(&repository));
        assert!(list.contains("r123"));
        assert!(list.contains("herdr"));
        let detail = format_repository(&repository);
        assert!(detail.contains("git common dir: /repo/.git"));
        assert!(detail.contains("preferred base: origin/main"));
    }
}
