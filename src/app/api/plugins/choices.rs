use std::collections::HashSet;
use std::fmt;

use serde::Deserialize;
use unicode_width::UnicodeWidthStr;

use crate::api::schema::{PluginActionChoice, PluginActionChoices};

pub(crate) const MAX_CHOICES: usize = 64;
pub(crate) const MAX_CHOICE_ID_CHARS: usize = 120;
pub(crate) const MAX_CHOICE_LABEL_CHARS: usize = 256;
pub(crate) const MAX_CHOICE_PAYLOAD_BYTES: usize = 8 * 1024;
pub(crate) const MAX_CHOICE_JSON_DEPTH: usize = 16;
pub(crate) const MAX_CHOICES_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginActionChoicesParseError {
    OutputTooLarge,
    InvalidUtf8,
    InvalidJson(String),
    UnsupportedVersion(u32),
    TooManyChoices,
    EmptyId,
    IdTooLong,
    DuplicateId(String),
    EmptyOrZeroWidthLabel,
    LabelTooLong,
    UnsafeLabel,
    PayloadTooLarge,
    PayloadTooDeep,
}

impl fmt::Display for PluginActionChoicesParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooLarge => formatter.write_str("choices output exceeds 64 KiB"),
            Self::InvalidUtf8 => formatter.write_str("choices output is not valid UTF-8"),
            Self::InvalidJson(error) => write!(formatter, "invalid choices JSON: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported choices output version {version}")
            }
            Self::TooManyChoices => {
                formatter.write_str("choices output contains more than 64 choices")
            }
            Self::EmptyId => formatter.write_str("choice id must not be empty"),
            Self::IdTooLong => formatter.write_str("choice id exceeds 120 characters"),
            Self::DuplicateId(id) => write!(formatter, "duplicate choice id '{id}'"),
            Self::EmptyOrZeroWidthLabel => {
                formatter.write_str("choice label must not be empty or zero-width-only")
            }
            Self::LabelTooLong => formatter.write_str("choice label exceeds 256 characters"),
            Self::UnsafeLabel => formatter.write_str("choice label contains unsafe characters"),
            Self::PayloadTooLarge => formatter.write_str("choice payload exceeds 8 KiB"),
            Self::PayloadTooDeep => formatter.write_str("choice payload exceeds JSON depth 16"),
        }
    }
}

impl std::error::Error for PluginActionChoicesParseError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChoicesOutput {
    version: u32,
    choices: Vec<RawChoice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChoice {
    id: String,
    label: String,
    payload: serde_json::Value,
}

/// Parse and validate the complete stdout of a plugin action choices provider.
pub(crate) fn parse_plugin_action_choices(
    output: &[u8],
) -> Result<PluginActionChoices, PluginActionChoicesParseError> {
    if output.len() > MAX_CHOICES_OUTPUT_BYTES {
        return Err(PluginActionChoicesParseError::OutputTooLarge);
    }
    let text =
        std::str::from_utf8(output).map_err(|_| PluginActionChoicesParseError::InvalidUtf8)?;
    let raw: RawChoicesOutput = serde_json::from_str(text)
        .map_err(|error| PluginActionChoicesParseError::InvalidJson(error.to_string()))?;

    if raw.version != 1 {
        return Err(PluginActionChoicesParseError::UnsupportedVersion(
            raw.version,
        ));
    }
    if raw.choices.len() > MAX_CHOICES {
        return Err(PluginActionChoicesParseError::TooManyChoices);
    }

    let mut ids = HashSet::with_capacity(raw.choices.len());
    let mut choices = Vec::with_capacity(raw.choices.len());
    for choice in raw.choices {
        validate_id(&choice.id, &mut ids)?;
        validate_label(&choice.label)?;
        validate_payload(&choice.payload)?;
        choices.push(PluginActionChoice {
            id: choice.id,
            label: choice.label,
            payload: choice.payload,
        });
    }

    Ok(PluginActionChoices {
        version: 1,
        choices,
    })
}

fn validate_id(id: &str, ids: &mut HashSet<String>) -> Result<(), PluginActionChoicesParseError> {
    if id.is_empty() {
        return Err(PluginActionChoicesParseError::EmptyId);
    }
    if id.chars().count() > MAX_CHOICE_ID_CHARS {
        return Err(PluginActionChoicesParseError::IdTooLong);
    }
    if !ids.insert(id.to_string()) {
        return Err(PluginActionChoicesParseError::DuplicateId(id.to_string()));
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), PluginActionChoicesParseError> {
    if label.chars().count() > MAX_CHOICE_LABEL_CHARS {
        return Err(PluginActionChoicesParseError::LabelTooLong);
    }
    if label.chars().any(is_unsafe_label_character) {
        return Err(PluginActionChoicesParseError::UnsafeLabel);
    }
    if label.is_empty() || UnicodeWidthStr::width(label) == 0 {
        return Err(PluginActionChoicesParseError::EmptyOrZeroWidthLabel);
    }
    Ok(())
}

fn is_unsafe_label_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn validate_payload(payload: &serde_json::Value) -> Result<(), PluginActionChoicesParseError> {
    if payload_exceeds_depth(payload, 0) {
        return Err(PluginActionChoicesParseError::PayloadTooDeep);
    }
    let serialized = serde_json::to_vec(payload)
        .map_err(|error| PluginActionChoicesParseError::InvalidJson(error.to_string()))?;
    if serialized.len() > MAX_CHOICE_PAYLOAD_BYTES {
        return Err(PluginActionChoicesParseError::PayloadTooLarge);
    }
    Ok(())
}

fn payload_exceeds_depth(value: &serde_json::Value, depth: usize) -> bool {
    match value {
        serde_json::Value::Array(values) => {
            let depth = depth + 1;
            depth > MAX_CHOICE_JSON_DEPTH
                || values
                    .iter()
                    .any(|value| payload_exceeds_depth(value, depth))
        }
        serde_json::Value::Object(values) => {
            let depth = depth + 1;
            depth > MAX_CHOICE_JSON_DEPTH
                || values
                    .values()
                    .any(|value| payload_exceeds_depth(value, depth))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn output(choices: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({ "version": 1, "choices": choices })).unwrap()
    }

    fn one_choice(id: &str, label: &str, payload: serde_json::Value) -> serde_json::Value {
        json!([{ "id": id, "label": label, "payload": payload }])
    }

    #[test]
    fn parses_version_one_document_and_preserves_provider_order() {
        let parsed = parse_plugin_action_choices(&output(json!([
            { "id": "second", "label": "Second", "payload": { "n": 2 } },
            { "id": "first", "label": "First", "payload": null }
        ])))
        .unwrap();

        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.choices[0].id, "second");
        assert_eq!(parsed.choices[1].payload, serde_json::Value::Null);
    }

    #[test]
    fn accepts_fixed_count_id_and_label_boundaries() {
        let choices = (0..MAX_CHOICES)
            .map(|index| {
                json!({
                    "id": format!("{index:02}{}", "i".repeat(MAX_CHOICE_ID_CHARS - 2)),
                    "label": "l".repeat(MAX_CHOICE_LABEL_CHARS),
                    "payload": null
                })
            })
            .collect::<Vec<_>>();

        let parsed = parse_plugin_action_choices(&output(json!(choices))).unwrap();
        assert_eq!(parsed.choices.len(), MAX_CHOICES);
    }

    #[test]
    fn accepts_payload_size_and_depth_boundaries() {
        let payload = serde_json::Value::String("x".repeat(MAX_CHOICE_PAYLOAD_BYTES - 2));
        parse_plugin_action_choices(&output(one_choice("id", "label", payload))).unwrap();

        let mut payload = serde_json::Value::Null;
        for _ in 0..MAX_CHOICE_JSON_DEPTH {
            payload = json!([payload]);
        }
        parse_plugin_action_choices(&output(one_choice("id", "label", payload))).unwrap();
    }

    #[test]
    fn rejects_invalid_utf8_trailing_documents_and_wrong_shapes() {
        assert_eq!(
            parse_plugin_action_choices(&[0xff]),
            Err(PluginActionChoicesParseError::InvalidUtf8)
        );
        assert!(matches!(
            parse_plugin_action_choices(br#"{"version":1,"choices":[]} {}"#),
            Err(PluginActionChoicesParseError::InvalidJson(_))
        ));
        for malformed in [
            br#"[]"#.as_slice(),
            br#"{"version":1}"#.as_slice(),
            br#"{"version":1,"choices":[],"extra":true}"#.as_slice(),
            br#"{"version":1,"choices":[{"id":"a","label":"A"}]}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_plugin_action_choices(malformed),
                Err(PluginActionChoicesParseError::InvalidJson(_))
            ));
        }
    }

    #[test]
    fn rejects_unsupported_version_excess_choices_and_duplicate_ids() {
        assert_eq!(
            parse_plugin_action_choices(br#"{"version":2,"choices":[]}"#),
            Err(PluginActionChoicesParseError::UnsupportedVersion(2))
        );
        let choices = (0..=MAX_CHOICES)
            .map(|index| json!({ "id": index.to_string(), "label": "x", "payload": null }))
            .collect::<Vec<_>>();
        assert_eq!(
            parse_plugin_action_choices(&output(json!(choices))),
            Err(PluginActionChoicesParseError::TooManyChoices)
        );
        assert_eq!(
            parse_plugin_action_choices(&output(json!([
                { "id": "same", "label": "One", "payload": null },
                { "id": "same", "label": "Two", "payload": null }
            ]))),
            Err(PluginActionChoicesParseError::DuplicateId("same".into()))
        );
    }

    #[test]
    fn rejects_id_and_label_overflows() {
        assert_eq!(
            parse_plugin_action_choices(&output(one_choice("", "label", json!(null)))),
            Err(PluginActionChoicesParseError::EmptyId)
        );
        assert_eq!(
            parse_plugin_action_choices(&output(one_choice(
                &"i".repeat(MAX_CHOICE_ID_CHARS + 1),
                "label",
                json!(null)
            ))),
            Err(PluginActionChoicesParseError::IdTooLong)
        );
        assert_eq!(
            parse_plugin_action_choices(&output(one_choice(
                "id",
                &"l".repeat(MAX_CHOICE_LABEL_CHARS + 1),
                json!(null)
            ))),
            Err(PluginActionChoicesParseError::LabelTooLong)
        );
    }

    #[test]
    fn rejects_control_bidi_and_zero_width_only_labels() {
        for label in [
            "line\nbreak",
            "tab\there",
            "escape\u{1b}",
            "right\u{202e}left",
        ] {
            assert_eq!(
                parse_plugin_action_choices(&output(one_choice("id", label, json!(null)))),
                Err(PluginActionChoicesParseError::UnsafeLabel)
            );
        }
        assert_eq!(
            parse_plugin_action_choices(&output(one_choice("id", "\u{200b}", json!(null)))),
            Err(PluginActionChoicesParseError::EmptyOrZeroWidthLabel)
        );
    }

    #[test]
    fn rejects_payload_size_and_depth_overflows() {
        let payload = serde_json::Value::String("x".repeat(MAX_CHOICE_PAYLOAD_BYTES - 1));
        assert_eq!(
            parse_plugin_action_choices(&output(one_choice("id", "label", payload))),
            Err(PluginActionChoicesParseError::PayloadTooLarge)
        );

        let mut payload = serde_json::Value::Null;
        for _ in 0..=MAX_CHOICE_JSON_DEPTH {
            payload = json!([payload]);
        }
        assert_eq!(
            parse_plugin_action_choices(&output(one_choice("id", "label", payload))),
            Err(PluginActionChoicesParseError::PayloadTooDeep)
        );
    }

    #[test]
    fn enforces_output_size_boundary() {
        let mut at_limit = output(json!([]));
        at_limit.resize(MAX_CHOICES_OUTPUT_BYTES, b' ');
        parse_plugin_action_choices(&at_limit).unwrap();

        assert_eq!(
            parse_plugin_action_choices(&vec![b' '; MAX_CHOICES_OUTPUT_BYTES + 1]),
            Err(PluginActionChoicesParseError::OutputTooLarge)
        );
    }
}
