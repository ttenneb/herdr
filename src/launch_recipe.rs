//! Durable launch recipe and Herdr-owned sleep state for managed agent panes.
//!
//! A recipe is recorded when a managed launch commits (`agent.start` or
//! `collection.helper_launch`) and persisted with the pane, so Herdr can later
//! relaunch the same agent in the same pane: to wake a pane it put to sleep, or
//! to resume it after a server restart. It holds only what the caller passed
//! explicitly: never the inherited environment, and never credentials.

use serde::{Deserialize, Serialize};

/// Reserved `--env` key a lifecycle role manager passes on its launches.
/// Herdr never sleeps or wakes such a pane; the role manager owns it.
pub(crate) const LIFECYCLE_ROLE_ENV: &str = "HERDR_LIFECYCLE_ROLE";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchRecipe {
    pub name: String,
    pub kind: String,
    /// Agent arguments after the executable, exactly as launched.
    pub args: Vec<String>,
    /// Only the `--env NAME=value` pairs the caller passed, minus
    /// credential-like names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_role: Option<String>,
}

/// Set only by `agent.sleep`; cleared when a Pi for the pane attaches to
/// Messages, or when an agent is started in the pane by anything but a wake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PaneSleep {
    pub since_ms: u64,
    pub agent_name: String,
    pub generation: u64,
}

/// A delegation report route that was ready for the pane's previous managed
/// generation. Only a recipe relaunch (wake or restart resume) of the same pane
/// on the same session file re-establishes it for the new generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RouteCarry {
    pub child_delegation: String,
    pub parent_delegation: String,
    pub session_path: String,
    pub generation: u64,
    /// The parent execution the route was ready with. A carry is refused if
    /// the parent pane now holds another terminal or another session.
    #[serde(default)]
    pub parent_terminal: String,
    #[serde(default)]
    pub parent_session: String,
}

/// Environment names a recipe may persist. Everything else passed with
/// `--env` is dropped, so a relaunch never replays a credential.
fn env_name_allowed(name: &str) -> bool {
    const EXPLICIT: &[&str] = &[LIFECYCLE_ROLE_ENV, "TERM", "LANG", "LC_ALL", "TZ"];
    let allowed = name.starts_with("PI_") || EXPLICIT.contains(&name);
    let upper = name.to_ascii_uppercase();
    let credential = [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
    ]
    .iter()
    .any(|needle| upper.contains(needle));
    allowed && !credential
}

/// A value such as `https://user:pass@host` carries a credential.
fn value_has_url_credentials(value: &str) -> bool {
    value.split("://").skip(1).any(|rest| {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        authority.contains('@')
    })
}

fn argument_carries_secret(arg: &str) -> bool {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    matches!(flag, "--api-key" | "--apiKey" | "--token")
}

/// Pi flags that always take the next argument as their value.
const PI_VALUE_FLAGS: &[&str] = &[
    "--api-key",
    "--append-system-prompt",
    "--export",
    "--fork",
    "--mode",
    "--model",
    "--models",
    "--prompt-template",
    "--provider",
    "--session",
    "--session-dir",
    "--session-id",
    "--skill",
    "--system-prompt",
    "--theme",
    "--thinking",
    "--extension",
    "-e",
    "--name",
    "-n",
    "--tools",
    "-t",
    "--exclude-tools",
    "-xt",
];
/// Pi flags that take the next argument only when it does not look like a flag.
const PI_OPTIONAL_VALUE_FLAGS: &[&str] = &["--list-models", "--use-theme", "--tui-mode"];
/// Pi flags that never take a value; a bare token after one is a message.
const PI_BOOLEAN_FLAGS: &[&str] = &[
    "-a",
    "--approve",
    "-c",
    "--continue",
    "-h",
    "--help",
    "-na",
    "--no-approve",
    "-nbt",
    "--no-builtin-tools",
    "-nc",
    "--no-context-files",
    "-ne",
    "--no-extensions",
    "--no-prompt-templates",
    "--no-session",
    "--no-skills",
    "--no-themes",
    "--no-tools",
    "-np",
    "-ns",
    "-nt",
    "--offline",
    "-r",
    "--resume",
    "-v",
    "--verbose",
    "--version",
];

/// Whether Pi would read part of `args` as an initial prompt: a positional
/// message, an `@file`, `-p`/`--print`, or anything after `--`. A recipe with
/// one would re-send it on every wake. Mirrors Pi 0.87's argument parser,
/// conservatively (an unclear shape counts as a prompt).
fn args_carry_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let next = args.get(index + 1).map(String::as_str);
        if arg == "--" {
            return index + 1 < args.len();
        }
        if arg.starts_with('@') || arg == "-p" || arg == "--print" {
            return true;
        }
        if PI_VALUE_FLAGS.contains(&arg) {
            index += 2;
            continue;
        }
        if PI_OPTIONAL_VALUE_FLAGS.contains(&arg)
            || (arg.starts_with("--") && !arg.contains('=') && !PI_BOOLEAN_FLAGS.contains(&arg))
        {
            // Unknown long flags also consume a following bare value.
            let consumes =
                next.is_some_and(|value| !value.starts_with('-') && !value.starts_with('@'));
            index += if consumes { 2 } else { 1 };
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return true;
    }
    false
}

impl LaunchRecipe {
    /// `None` when the launch cannot be recorded without keeping a secret
    /// (for example `--api-key` on the command line).
    pub(crate) fn capture(
        name: &str,
        kind: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Option<Self> {
        if args
            .iter()
            .any(|arg| argument_carries_secret(arg) || value_has_url_credentials(arg))
            || args_carry_prompt(args)
        {
            return None;
        }
        let lifecycle_role = env
            .iter()
            .find(|(key, _)| key == LIFECYCLE_ROLE_ENV)
            .map(|(_, value)| value.clone());
        let env = env
            .iter()
            .filter(|(key, value)| env_name_allowed(key) && !value_has_url_credentials(value))
            .cloned()
            .collect();
        Some(Self {
            name: name.to_string(),
            kind: kind.to_string(),
            args: args.to_vec(),
            env,
            lifecycle_role,
        })
    }

    /// The recipe in `agent.start` parameter form.
    pub(crate) fn env_assignments(&self) -> Vec<String> {
        self.env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn capture_keeps_only_explicit_non_credential_env() {
        let recipe = LaunchRecipe::capture(
            "owner",
            "pi",
            &["--session".into(), "/s.jsonl".into()],
            &pairs(&[
                ("PI_CODING_AGENT_DIR", "/a"),
                ("ANTHROPIC_API_KEY", "sk-secret"),
                ("GITHUB_TOKEN", "t"),
                ("OPENAI_BASE_URL", "https://api.example"),
                ("HERDR_LIFECYCLE_ROLE", "owner-1"),
            ]),
        )
        .unwrap();
        assert_eq!(
            recipe.env,
            pairs(&[
                ("PI_CODING_AGENT_DIR", "/a"),
                ("HERDR_LIFECYCLE_ROLE", "owner-1")
            ])
        );
        assert_eq!(recipe.lifecycle_role.as_deref(), Some("owner-1"));
        assert_eq!(
            recipe.env_assignments(),
            ["PI_CODING_AGENT_DIR=/a", "HERDR_LIFECYCLE_ROLE=owner-1"]
        );
    }

    #[test]
    fn capture_allowlists_env_and_drops_url_credentials() {
        let recipe = LaunchRecipe::capture(
            "owner",
            "pi",
            &[],
            &pairs(&[
                ("PI_CODING_AGENT_DIR", "/a"),
                ("PI_TASKING_HERDR_ADAPTER_CONFIG", "/cfg.json"),
                ("HERDR_LIFECYCLE_ROLE", "owner-1"),
                ("LANG", "C.UTF-8"),
                ("PATH", "/usr/bin"),
                ("HTTPS_PROXY", "http://proxy:8080"),
                ("PI_REGISTRY", "https://user:pass@registry.example/npm"),
                ("PI_AUTH_TOKEN", "x"),
                ("OPENAI_BASE_URL", "https://api.example"),
            ]),
        )
        .unwrap();
        assert_eq!(
            recipe.env,
            pairs(&[
                ("PI_CODING_AGENT_DIR", "/a"),
                ("PI_TASKING_HERDR_ADAPTER_CONFIG", "/cfg.json"),
                ("HERDR_LIFECYCLE_ROLE", "owner-1"),
                ("LANG", "C.UTF-8"),
            ])
        );
        assert_eq!(
            LaunchRecipe::capture(
                "a",
                "pi",
                &["--model".into(), "https://u:p@host/m".into()],
                &[]
            ),
            None
        );
    }

    #[test]
    fn capture_refuses_positional_prompts_files_and_print() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        for refused in [
            &["--session", "s.jsonl", "do X"][..],
            &["do X"],
            &["--", "do X"],
            &["--session", "s.jsonl", "@task.md"],
            &["-p", "summarize"],
            &["--print"],
            &["--verbose", "do X"],
            &["-c", "continue with X"],
        ] {
            assert_eq!(
                LaunchRecipe::capture("a", "pi", &args(refused), &[]),
                None,
                "{refused:?}"
            );
        }
        for accepted in [
            &[
                "--session",
                "s.jsonl",
                "--thinking",
                "low",
                "--exclude-tools",
                "ask_user_question",
            ][..],
            &["--name", "owner", "--model", "anthropic/claude"],
            &["--verbose", "--offline"],
            &["--some-extension-flag", "value", "--session=s.jsonl"],
            &["--"],
        ] {
            assert!(
                LaunchRecipe::capture("a", "pi", &args(accepted), &[]).is_some(),
                "{accepted:?}"
            );
        }
    }

    #[test]
    fn capture_refuses_secret_arguments() {
        for arg in ["--api-key", "--api-key=sk"] {
            assert_eq!(
                LaunchRecipe::capture("a", "pi", &[arg.into(), "x".into()], &[]),
                None
            );
        }
    }
}
