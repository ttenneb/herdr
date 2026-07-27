use std::collections::HashMap;

use crate::api::schema::*;

pub(super) fn run_collection_command(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return usage(2);
    };
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
    let mut target_pane_id = None;
    let mut focus_promoted = false;
    parse_options(&args[1..], |name, value| match name {
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
        "--target-pane" => {
            target_pane_id = Some(required_value(name, value)?);
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
        target_pane_id,
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
            let (key, value) = super::parse_env_assignment(&raw)?;
            env.insert(key, value);
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
    eprintln!("herdr collection commands: list, get, create, add, move, promote, select, reorder, archive, restore, member-create, close");
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
            "--target-pane",
            "w1:p1",
            "--focus-promoted",
        ]))
        .expect("close");
        assert_eq!(
            close.disposition,
            Some(CollectionCloseDisposition::PromoteMembers)
        );
        assert!(close.focus_promoted);

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
