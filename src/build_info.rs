//! Build identity helpers.

use serde::{Deserialize, Serialize};

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildIdentity {
    pub version: String,
    pub channel: String,
    pub build_id: Option<String>,
    pub source_commit: Option<String>,
}

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn source_commit() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_COMMIT"))
}

pub fn identity() -> BuildIdentity {
    BuildIdentity {
        version: version(),
        channel: channel().to_string(),
        build_id: build_id().map(str::to_string),
        source_commit: source_commit().map(str::to_string),
    }
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }

    #[test]
    fn surfaced_identity_includes_version_channel_build_and_source_revision() {
        let identity = super::identity();
        assert_eq!(identity.version, super::version());
        assert_eq!(identity.channel, super::channel());
        assert_eq!(identity.build_id.as_deref(), super::build_id());
        assert_eq!(identity.source_commit.as_deref(), super::source_commit());
        let encoded = serde_json::to_value(identity).unwrap();
        assert!(encoded.get("version").is_some());
        assert!(encoded.get("channel").is_some());
        assert!(encoded.get("buildId").is_some());
        assert!(encoded.get("sourceCommit").is_some());
    }
}
