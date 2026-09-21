use std::path::Path;
use std::process::Command;

pub(super) fn run_mailbox_command(args: &[String]) -> std::io::Result<i32> {
    let Some(params) = parse_dispatch(args) else {
        print_usage();
        return Ok(2);
    };
    let status = Command::new("python3")
        .arg(&params.router)
        .arg("--durable-root")
        .arg(&params.durable_root)
        .arg("--manifest")
        .arg(&params.manifest)
        .arg("--request")
        .arg(&params.request)
        .status()?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Debug, PartialEq, Eq)]
struct DispatchParams {
    router: String,
    manifest: String,
    request: String,
    durable_root: String,
}

fn parse_dispatch(args: &[String]) -> Option<DispatchParams> {
    let [command, rest @ ..] = args else {
        return None;
    };
    if command != "dispatch" {
        return None;
    }
    let mut router = None;
    let mut manifest = None;
    let mut request = None;
    let mut durable_root = None;
    let mut index = 0;
    while index < rest.len() {
        let value = rest.get(index + 1)?.clone();
        match rest[index].as_str() {
            "--router" => router = Some(value),
            "--manifest" => manifest = Some(value),
            "--request" => request = Some(value),
            "--durable-root" => durable_root = Some(value),
            _ => return None,
        }
        index += 2;
    }
    let params = DispatchParams {
        router: router?,
        manifest: manifest?,
        request: request?,
        durable_root: durable_root?,
    };
    let all_absolute = [
        &params.router,
        &params.manifest,
        &params.request,
        &params.durable_root,
    ]
    .into_iter()
    .all(|value| Path::new(value).is_absolute());
    all_absolute.then_some(params)
}

fn print_usage() {
    eprintln!(
        "usage: herdr mailbox dispatch --router PATH --manifest PATH --request PATH --durable-root PATH"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_requires_exact_absolute_paths() {
        let parsed = parse_dispatch(&[
            "dispatch".into(),
            "--router".into(),
            "/router.py".into(),
            "--manifest".into(),
            "/manifest.json".into(),
            "--request".into(),
            "/request.json".into(),
            "--durable-root".into(),
            "/durable".into(),
        ]);
        assert_eq!(
            parsed,
            Some(DispatchParams {
                router: "/router.py".into(),
                manifest: "/manifest.json".into(),
                request: "/request.json".into(),
                durable_root: "/durable".into(),
            })
        );
        assert!(
            parse_dispatch(&["dispatch".into(), "--router".into(), "relative.py".into()]).is_none()
        );
    }
}
