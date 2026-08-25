use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::toolbox::{ToolboxExecutionError, ToolboxTool};

pub const TOOL_NAME: &str = "CurrentTime";
pub const TOOLBOX_NAME: &str = "CurrentTime";

const TOOLBOX_BRIEF: &str = r#"CurrentTime returns the host's current local and UTC time.

Call CurrentTime whenever the user asks about the current date or time, a relative date such as today or tomorrow, or any answer that depends on what time it is now. Query it freshly for each such question instead of guessing or reusing an earlier result."#;
const INSTRUCTIONS: &str = r#"Call with an empty object. The result contains the same instant as local and UTC RFC3339 timestamps, the local UTC offset, the local weekday, and Unix time in milliseconds. Use the returned values as the authority for current and relative date/time reasoning."#;
const ROUTE: &str = "Get the host's current local and UTC time for questions whose answer depends on the present date or time.";
const EXAMPLES: &str = r#"Input: {}
Meaning: read the host clock before answering a current or relative date/time question."#;

pub fn catalog_parts() -> (Vec<ToolboxTool>, (String, String)) {
    (
        vec![ToolboxTool {
            toolbox: TOOLBOX_NAME.into(),
            local_name: TOOL_NAME.into(),
            full_name: TOOL_NAME.into(),
            api_name: TOOL_NAME.into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "required": [
                    "local_rfc3339",
                    "utc_rfc3339",
                    "utc_offset",
                    "weekday",
                    "unix_timestamp_ms"
                ],
                "properties": {
                    "local_rfc3339": {"type": "string"},
                    "utc_rfc3339": {"type": "string"},
                    "utc_offset": {"type": "string"},
                    "weekday": {"type": "string"},
                    "unix_timestamp_ms": {"type": "integer"}
                },
                "additionalProperties": false
            }),
            instructions: INSTRUCTIONS.into(),
            route: ROUTE.into(),
            examples: EXAMPLES.into(),
        }],
        (TOOLBOX_NAME.into(), TOOLBOX_BRIEF.into()),
    )
}

pub fn execute(arguments: &str) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: Value =
        serde_json::from_str(arguments).map_err(|error| ToolboxExecutionError::Tool {
            code: "invalid_arguments".into(),
            message: error.to_string(),
            retryable: false,
            tip: None,
        })?;
    if input.as_object().is_none_or(|object| !object.is_empty()) {
        return Err(ToolboxExecutionError::Tool {
            code: "invalid_arguments".into(),
            message: "CurrentTime accepts only an empty object".into(),
            retryable: false,
            tip: None,
        });
    }
    Ok(format_time(Utc::now()))
}

fn format_time(now: DateTime<Utc>) -> Value {
    let local = now.with_timezone(&Local);
    json!({
        "local_rfc3339": local.to_rfc3339_opts(SecondsFormat::Millis, false),
        "utc_rfc3339": now.to_rfc3339_opts(SecondsFormat::Millis, true),
        "utc_offset": local.format("%:z").to_string(),
        "weekday": local.format("%A").to_string(),
        "unix_timestamp_ms": now.timestamp_millis(),
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn formats_one_instant_with_complete_unambiguous_fields() {
        let now = Utc.with_ymd_and_hms(2026, 5, 25, 1, 2, 3).unwrap();
        let value = format_time(now);
        assert_eq!(value["utc_rfc3339"], "2026-05-25T01:02:03.000Z");
        assert_eq!(value["unix_timestamp_ms"], now.timestamp_millis());
        assert!(value["local_rfc3339"].as_str().unwrap().contains('T'));
        assert_eq!(value["utc_offset"].as_str().unwrap().len(), 6);
        assert!(!value["weekday"].as_str().unwrap().is_empty());
    }

    #[test]
    fn rejects_every_nonempty_or_nonobject_input() {
        assert!(execute("{}").is_ok());
        assert!(execute(r#"{"timezone":"UTC"}"#).is_err());
        assert!(execute("[]").is_err());
        assert!(execute("not json").is_err());
    }

    #[test]
    fn catalog_exposes_the_exact_top_level_tool_contract() {
        let (tools, (_, brief)) = catalog_parts();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].full_name, TOOL_NAME);
        assert_eq!(tools[0].api_name, TOOL_NAME);
        assert_eq!(tools[0].input_schema["additionalProperties"], false);
        assert!(brief.contains("Query it freshly"));
        assert!(brief.contains("relative date"));
    }
}
