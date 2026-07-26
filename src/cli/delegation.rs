use crate::api::schema::*;

pub(super) fn run_delegation_command(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return usage(2);
    };
    let method = match command {
        "create" => parse_create(&args[1..]).map(Method::DelegationCreate),
        "get" => one(&args[1..])
            .map(|delegation_id| Method::DelegationGet(DelegationTarget { delegation_id })),
        "tree" if args.len() == 1 => Ok(Method::DelegationTree(EmptyParams::default())),
        "root" => one(&args[1..])
            .map(|delegation_id| Method::DelegationRoot(DelegationTarget { delegation_id })),
        "descendants" => one(&args[1..])
            .map(|delegation_id| Method::DelegationDescendants(DelegationTarget { delegation_id })),
        "reparent" => parse_reparent(&args[1..]).map(Method::DelegationReparent),
        "reorder" => parse_reorder(&args[1..]).map(Method::DelegationReorder),
        "help" | "--help" | "-h" => return usage(0),
        _ => return usage(2),
    };
    match method {
        Ok(method) => super::runtime::delegation(method),
        Err(message) => {
            eprintln!("{message}");
            usage(2)
        }
    }
}
fn parse_create(args: &[String]) -> Result<DelegationCreateParams, String> {
    let mut pane_id = None;
    let mut parent_id = None;
    let mut purpose = None;
    let mut i = 0;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing value for {}", args[i]))?
            .clone();
        match args[i].as_str() {
            "--pane" => pane_id = Some(value),
            "--parent" => parent_id = Some(value),
            "--purpose" => purpose = Some(value),
            other => return Err(format!("unknown option: {other}")),
        }
        i += 2;
    }
    Ok(DelegationCreateParams {
        pane_id,
        parent_id,
        purpose,
    })
}
fn parse_reparent(args: &[String]) -> Result<DelegationReparentParams, String> {
    let delegation_id = args.first().cloned().ok_or("missing delegation_id")?;
    let mut parent_id = None;
    match &args[1..] {
        [flag] if flag == "--root" => {}
        [flag, value] if flag == "--parent" => parent_id = Some(value.clone()),
        _ => return Err("usage: herdr delegation reparent <id> (--parent ID|--root)".into()),
    }
    Ok(DelegationReparentParams {
        delegation_id,
        parent_id,
    })
}
fn parse_reorder(args: &[String]) -> Result<DelegationReorderParams, String> {
    let delegation_id = args.first().cloned().ok_or("missing delegation_id")?;
    let position = match &args[1..] {
        [flag] if flag == "--first" => DelegationSiblingPosition::First,
        [flag] if flag == "--last" => DelegationSiblingPosition::Last,
        [flag, value] if flag == "--before" => DelegationSiblingPosition::Before {
            delegation_id: value.clone(),
        },
        [flag, value] if flag == "--after" => DelegationSiblingPosition::After {
            delegation_id: value.clone(),
        },
        _ => return Err("choose --first, --last, --before ID, or --after ID".into()),
    };
    Ok(DelegationReorderParams {
        delegation_id,
        position,
    })
}
fn one(args: &[String]) -> Result<String, String> {
    if args.len() == 1 {
        Ok(args[0].clone())
    } else {
        Err("expected one delegation ID".into())
    }
}
fn usage(code: i32) -> std::io::Result<i32> {
    eprintln!("herdr delegation commands: create, get, tree, root, descendants, reparent, reorder");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_delegation_create_reparent_and_all_reorder_positions() {
        let create = parse_create(&strings(&[
            "--pane",
            "w1:p1",
            "--parent",
            "d1",
            "--purpose",
            "review",
        ]))
        .expect("create");
        assert_eq!(create.parent_id.as_deref(), Some("d1"));
        assert_eq!(
            parse_reparent(&strings(&["d2", "--root"]))
                .expect("root")
                .parent_id,
            None
        );
        assert_eq!(
            parse_reparent(&strings(&["d2", "--parent", "d1"]))
                .expect("parent")
                .parent_id
                .as_deref(),
            Some("d1")
        );
        assert!(matches!(
            parse_reorder(&strings(&["d2", "--first"]))
                .expect("first")
                .position,
            DelegationSiblingPosition::First
        ));
        assert!(matches!(
            parse_reorder(&strings(&["d2", "--last"]))
                .expect("last")
                .position,
            DelegationSiblingPosition::Last
        ));
        assert!(matches!(
            parse_reorder(&strings(&["d2", "--before", "d1"]))
                .expect("before")
                .position,
            DelegationSiblingPosition::Before { .. }
        ));
        assert!(matches!(
            parse_reorder(&strings(&["d2", "--after", "d1"]))
                .expect("after")
                .position,
            DelegationSiblingPosition::After { .. }
        ));
    }

    #[test]
    fn delegation_parsers_reject_ambiguous_arguments() {
        assert!(parse_reparent(&strings(&["d2", "--root", "--parent", "d1"])).is_err());
        assert!(parse_reorder(&strings(&["d2", "--first", "--last"])).is_err());
        assert!(parse_create(&strings(&["--pane"])).is_err());
    }
}
