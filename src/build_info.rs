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
    version_for(BASE_VERSION, channel(), build_id())
}

fn version_for(base: &str, channel: &str, build_id: Option<&str>) -> String {
    match channel {
        "stable" => base.to_string(),
        channel => match build_id {
            Some(build_id) => format!("{base}-{channel}.{build_id}"),
            None => format!("{base}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

/// A channel other than the upstream `stable` and `preview` lines, such as a
/// locally maintained `stabilized` build (`HERDR_BUILD_CHANNEL=stabilized`).
/// Such a build is not on the upstream update path: upstream releases must not
/// be offered or installed over it.
pub fn is_custom_channel_name(channel: &str) -> bool {
    !matches!(channel, "stable" | "preview")
}

pub fn is_custom_channel() -> bool {
    is_custom_channel_name(channel())
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
    fn custom_channel_version_names_the_channel_and_build() {
        assert_eq!(super::version_for("0.8.4", "stable", Some("x")), "0.8.4");
        assert_eq!(
            super::version_for("0.8.4", "stabilized", Some("rc3.4a1ba76")),
            "0.8.4-stabilized.rc3.4a1ba76"
        );
        assert_eq!(
            super::version_for("0.8.4", "stabilized", None),
            "0.8.4-stabilized"
        );
        assert!(super::is_custom_channel_name("stabilized"));
        assert!(!super::is_custom_channel_name("stable"));
        assert!(!super::is_custom_channel_name("preview"));
    }

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
