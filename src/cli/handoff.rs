use std::io::Read;

use crate::api::schema::{HandoffSendParams, HerdrHandoff, Method, Request};

pub(super) fn run_handoff_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("validate") if args.len() == 2 => validate(&args[1]),
        Some("send") if args.len() == 2 => send(&args[1]),
        Some("help" | "--help" | "-h") => {
            print_help();
            Ok(0)
        }
        _ => {
            print_help();
            Ok(2)
        }
    }
}

fn read_envelope(source: &str) -> Result<HerdrHandoff, String> {
    let text = if source == "-" {
        let mut value = String::new();
        std::io::stdin()
            .read_to_string(&mut value)
            .map_err(|err| err.to_string())?;
        value
    } else {
        let path = std::path::Path::new(source);
        if path.is_file() {
            std::fs::read_to_string(path).map_err(|err| format!("cannot read {source}: {err}"))?
        } else {
            source.to_string()
        }
    };
    serde_json::from_str(&text).map_err(|err| format!("invalid handoff JSON: {err}"))
}

fn validate(source: &str) -> std::io::Result<i32> {
    let envelope = match read_envelope(source) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{err}");
            return Ok(1);
        }
    };
    if let Err(err) = envelope.validate() {
        eprintln!("invalid handoff: {err}");
        return Ok(1);
    }
    println!(
        "{}",
        serde_json::json!({
            "version": 1,
            "valid": true,
            "messageId": envelope.message_id,
            "encodedBytes": serde_json::to_vec(&envelope).expect("validated handoff serializes").len(),
        })
    );
    Ok(0)
}

fn send(source: &str) -> std::io::Result<i32> {
    let envelope = match read_envelope(source) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{err}");
            return Ok(1);
        }
    };
    if let Err(err) = envelope.validate() {
        eprintln!("invalid handoff: {err}");
        return Ok(1);
    }
    let response = super::send_request(&Request {
        id: "cli:handoff:send".into(),
        method: Method::HandoffSend(HandoffSendParams {
            envelope,
            send: Default::default(),
        }),
    })?;
    let admitted =
        response["result"]["receipt"]["outcome"].as_str() == Some("runtime_transaction_admitted");
    let code = super::print_response(&response)?;
    Ok(if code == 0 && !admitted { 1 } else { code })
}

fn print_help() {
    eprintln!("herdr handoff commands:");
    eprintln!("  herdr handoff validate <JSON|PATH|->");
    eprintln!("  herdr handoff send <JSON|PATH|->");
}

#[cfg(test)]
mod tests {
    #[test]
    fn malformed_json_is_rejected() {
        assert!(super::read_envelope("{").is_err());
    }
}
