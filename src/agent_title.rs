use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    event::{AgentTitleChangedEvent, Event, EventDataBase, EventId},
    toolbox::{DEFAULT_TOOL_RESULT_TOKEN_LIMIT, ToolboxExecutionError, ToolboxTool},
};

pub const TOOL_NAME: &str = "SetTitle";
pub const TOOLBOX_NAME: &str = "SetTitle";
pub const MAX_TITLE_CHARS: usize = 80;
pub const SUCCESS_MESSAGE: &str = "你已完成标题设置，接下来除非用户要求，否则不要再设置标题了";
pub const FIRST_USER_PROMPT_REMINDER: &str = "This is the user's first message in this conversation. Your first action for this message must be exactly one SetTitle call with a concise title based on the user's intent. Do not batch SetTitle with another tool. After SetTitle succeeds, continue handling the same user request.";

const TITLE_PROMPT: &str = r#"# Conversation title

Set a title for your conversation with the user. If this is the user's first message, you must use SetTitle once to set a title based on the user's message. If you have already set a title, do not proactively set it again unless the user asks you to change it."#;
const TOOLBOX_BRIEF: &str = r#"SetTitle assigns a concise human-readable title to the conversation.

On the user's first message, use SetTitle once to set a title based on that message. After setting it, do not proactively use SetTitle again unless the user asks you to change the title."#;
const INSTRUCTIONS: &str = r#"Provide one short, single-line title. Do not include an Agent ID, status prefix, quotation marks, trailing punctuation, or transient progress. The title must contain 1 to 80 characters."#;
const ROUTE: &str = "On the user's first message, set the conversation title once based on that message. Afterward, use this tool only when the user asks to change the title.";
const EXAMPLES: &str = r#"Input: {"title":"实现配置导入导出"}
Meaning: name the Agent after the user's current objective.

Input: {"title":"调查终端输入延迟"}
Meaning: use a concise task-oriented title, not a progress report."#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetTitleInput {
    title: String,
}

pub fn catalog_parts() -> (Vec<ToolboxTool>, (String, String)) {
    (
        vec![ToolboxTool {
            toolbox: TOOLBOX_NAME.into(),
            local_name: TOOL_NAME.into(),
            full_name: TOOL_NAME.into(),
            api_name: TOOL_NAME.into(),
            input_schema: json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_TITLE_CHARS
                    }
                },
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "string",
                "description": SUCCESS_MESSAGE
            }),
            result_token_limit: DEFAULT_TOOL_RESULT_TOKEN_LIMIT,
            instructions: INSTRUCTIONS.into(),
            route: ROUTE.into(),
            examples: EXAMPLES.into(),
        }],
        (TOOLBOX_NAME.into(), TOOLBOX_BRIEF.into()),
    )
}

pub fn execute(
    arguments: &str,
    tool_call_id: EventId,
    edb: &mut EventDataBase,
) -> std::result::Result<Value, ToolboxExecutionError> {
    let input: SetTitleInput =
        serde_json::from_str(arguments).map_err(|error| ToolboxExecutionError::Tool {
            code: "invalid_arguments".into(),
            message: error.to_string(),
            retryable: false,
            tip: None,
        })?;
    let title = normalize_title(&input.title).map_err(|message| ToolboxExecutionError::Tool {
        code: "invalid_title".into(),
        message,
        retryable: false,
        tip: None,
    })?;
    edb.append_agent_title_changed(tool_call_id, title)
        .map_err(|error| ToolboxExecutionError::Protocol(error.to_string()))?;
    Ok(Value::String(SUCCESS_MESSAGE.into()))
}

pub fn normalize_title(value: &str) -> std::result::Result<String, String> {
    let title = value.trim();
    if title.is_empty() {
        return Err("title cannot be empty".into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("title cannot exceed {MAX_TITLE_CHARS} characters"));
    }
    if title.chars().any(char::is_control) {
        return Err("title must be a single line without control characters".into());
    }
    Ok(title.to_owned())
}

pub fn current_title(events: &[Event]) -> Option<&str> {
    events.iter().rev().find_map(|event| match event {
        Event::AgentTitleChanged(changed) => Some(changed.title.as_str()),
        _ => None,
    })
}

pub fn persisted_change_result(events: &[Event], tool_call_id: EventId) -> Option<Value> {
    events.iter().find_map(|event| match event {
        Event::AgentTitleChanged(AgentTitleChangedEvent {
            tool_call_id: event_tool_call_id,
            ..
        }) if *event_tool_call_id == tool_call_id => Some(Value::String(SUCCESS_MESSAGE.into())),
        _ => None,
    })
}

pub fn system_prompt() -> &'static str {
    TITLE_PROMPT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_normalization_is_strict_and_unicode_aware() {
        assert_eq!(normalize_title("  调查终端问题  ").unwrap(), "调查终端问题");
        assert!(normalize_title("\n").is_err());
        assert!(normalize_title("two\nlines").is_err());
        assert!(normalize_title(&"界".repeat(MAX_TITLE_CHARS + 1)).is_err());
    }

    #[test]
    fn catalog_exposes_exact_top_level_tool_name() {
        let (tools, _) = catalog_parts();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].full_name, TOOL_NAME);
        assert_eq!(tools[0].api_name, TOOL_NAME);
        assert_eq!(tools[0].output_schema["type"], "string");
        assert_eq!(tools[0].output_schema["description"], SUCCESS_MESSAGE);
        assert!(system_prompt().contains("If this is the user's first message"));
        assert!(system_prompt().contains("do not proactively set it again"));
    }
}
