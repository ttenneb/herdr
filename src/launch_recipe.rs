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

fn credential_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "KEY",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
        "SESSION_ID",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

fn argument_carries_secret(arg: &str) -> bool {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    matches!(flag, "--api-key" | "--apiKey" | "--token")
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
        if args.iter().any(|arg| argument_carries_secret(arg)) {
            return None;
        }
        let lifecycle_role = env
            .iter()
            .find(|(key, _)| key == LIFECYCLE_ROLE_ENV)
            .map(|(_, value)| value.clone());
        let env = env
            .iter()
            .filter(|(key, _)| key == LIFECYCLE_ROLE_ENV || !credential_like(key))
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
    fn capture_refuses_secret_arguments() {
        for arg in ["--api-key", "--api-key=sk"] {
            assert_eq!(
                LaunchRecipe::capture("a", "pi", &[arg.into(), "x".into()], &[]),
                None
            );
        }
    }
}
