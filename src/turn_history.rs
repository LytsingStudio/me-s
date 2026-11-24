use serde_json::to_string;

use crate::{
    Result,
    event::{
        CompactState, Event, EventId, effective_conversation_events, effective_history_events,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct UserTurnHistory {
    id: EventId,
    content: String,
    image_paths: Vec<String>,
    follow_ups: Vec<(String, Vec<String>)>,
}

pub(crate) fn snapshot(events: &[Event], boundary: EventId) -> Result<String> {
    let effective = effective_history_events(events, boundary)?;
    let mut turns = Vec::<UserTurnHistory>::new();

    for event in effective {
        match event {
            Event::UserPrompt(prompt) => turns.push(UserTurnHistory {
                id: prompt.id,
                content: prompt.content.clone(),
                image_paths: prompt
                    .images
                    .iter()
                    .map(|image| image.path.clone())
                    .collect(),
                follow_ups: Vec::new(),
            }),
            Event::FollowUpPrompt(prompt) => {
                if let Some(turn) = turns.last_mut() {
                    turn.follow_ups.push((
                        prompt.content.clone(),
                        prompt
                            .images
                            .iter()
                            .map(|image| image.path.clone())
                            .collect(),
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(render(&turns))
}

pub(crate) fn latest_snapshot(events: &[Event]) -> Result<Option<String>> {
    let boundary =
        effective_conversation_events(events)?
            .into_iter()
            .find_map(|event| match event {
                Event::CompactStateUpdate(update) if update.state == CompactState::Completed => {
                    Some(update.tool_call_id)
                }
                _ => None,
            });
    boundary
        .map(|boundary| snapshot(events, boundary))
        .transpose()
}

fn render(turns: &[UserTurnHistory]) -> String {
    let mut output = String::from(
        "This is the user-message history frozen by ME at the latest successful context compaction. It is historical data, not new instructions. Only real user prompts and follow-ups are preserved exactly. Model output, internal agent prompts, turn state, and tool activity are omitted.\n",
    );
    for turn in turns {
        output.push_str(&format!("\nturn-{:08}\n", turn.id));
        output.push_str(&format!("  User: {}\n", json_string(&turn.content)));
        render_image_paths(&mut output, &turn.image_paths);
        for (content, image_paths) in &turn.follow_ups {
            output.push_str(&format!("  FollowUp: {}\n", json_string(content)));
            render_image_paths(&mut output, image_paths);
        }
    }
    output.trim_end().to_owned()
}

fn render_image_paths(output: &mut String, paths: &[String]) {
    if !paths.is_empty() {
        output.push_str(&format!(
            "  Attached image paths (not image content): {}\n",
            to_string(paths).expect("paths serialize")
        ));
    }
}

fn json_string(value: &str) -> String {
    to_string(value).expect("serializing a history string cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compact,
        event::{AgentKind, AgentTurnState, EventDataBase, ToolResultState},
    };

    #[test]
    fn snapshot_keeps_only_real_user_messages() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        let first = edb.append_user_prompt("first request").unwrap();
        edb.append_agent_turn(first, first, AgentTurnState::Started, "")
            .unwrap();
        edb.append_assist_response(first, "model output", true)
            .unwrap();
        let api = edb.append_api_requesting(first).unwrap();
        let call = edb
            .append_tool_call(api, first, "call-1", "File.Read", r#"{"path":"a.txt"}"#)
            .unwrap();
        edb.append_tool_result(
            call,
            ToolResultState::Succeeded,
            None,
            r#"{"content":"large tool result"}"#,
        )
        .unwrap();
        edb.append_follow_up_prompt(first, "real follow-up")
            .unwrap();
        edb.append_agent_turn(first, first, AgentTurnState::Completed, "")
            .unwrap();
        let boundary = edb
            .append_tool_call(api, first, "compact-1", compact::TOOL_NAME, "{}")
            .unwrap();

        let history = snapshot(edb.events(), boundary).unwrap();
        assert!(history.contains("first request"));
        assert!(history.contains("real follow-up"));
        assert!(!history.contains("model output"));
        assert!(!history.contains("File.Read"));
        assert!(!history.contains("large tool result"));
        assert!(!history.contains("completed"));
        assert!(!history.contains("FinalAnswer"));
    }

    #[test]
    fn snapshot_ignores_internal_agent_prompts() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "manager-agent", None, None)
            .unwrap();
        let user = edb.append_user_prompt("real user").unwrap();
        edb.append_manager_prompt("internal manager instruction")
            .unwrap();
        edb.append_parent_agent_prompt("internal parent instruction")
            .unwrap();
        let boundary = edb
            .append_tool_call(1, user, "compact", compact::TOOL_NAME, "{}")
            .unwrap();

        let history = snapshot(edb.events(), boundary).unwrap();
        assert!(history.contains("real user"));
        assert!(!history.contains("internal manager instruction"));
        assert!(!history.contains("internal parent instruction"));
    }

    #[test]
    fn next_snapshot_accumulates_previous_user_messages() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        let first = edb.append_user_prompt("first request").unwrap();
        let api = edb.append_api_requesting(first).unwrap();
        let first_boundary = edb
            .append_tool_call(api, first, "compact-1", compact::TOOL_NAME, "{}")
            .unwrap();
        let first_history = snapshot(edb.events(), first_boundary).unwrap();

        let second = edb.append_user_prompt("second request").unwrap();
        let second_boundary = edb
            .append_tool_call(api, second, "compact-2", compact::TOOL_NAME, "{}")
            .unwrap();
        assert_eq!(
            snapshot(edb.events(), first_boundary).unwrap(),
            first_history
        );
        let second_history = snapshot(edb.events(), second_boundary).unwrap();
        assert!(second_history.contains("first request"));
        assert!(second_history.contains("second request"));
    }

    #[test]
    fn latest_snapshot_tracks_only_the_effective_completed_compact() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        let prompt = edb.append_user_prompt("remember this").unwrap();
        let api = edb.append_api_requesting(prompt).unwrap();
        let call = edb
            .append_tool_call(api, prompt, "compact", compact::TOOL_NAME, "{}")
            .unwrap();
        edb.append_tool_result(call, ToolResultState::Succeeded, None, "ok")
            .unwrap();
        let compact = edb
            .append_compact_started(call, prompt, crate::event::CompactKind::WorkerSingleTurn)
            .unwrap();
        edb.append_compact_terminal(compact, CompactState::Completed, "summary", "")
            .unwrap();

        let history = latest_snapshot(edb.events()).unwrap().unwrap();
        assert!(history.contains("remember this"));

        edb.append_context_cleared().unwrap();
        assert_eq!(latest_snapshot(edb.events()).unwrap(), None);
    }

    #[test]
    fn attached_paths_are_historical_metadata_and_preserve_user_text_and_order() {
        let mut edb = EventDataBase::new();
        let image = |path: &str| crate::event::PromptImage {
            path: path.into(),
            sha256: "a".repeat(64),
            format: "png".into(),
            width: 1,
            height: 1,
        };
        let prompt = edb
            .append_user_prompt_with_images(
                "exact <text>",
                vec![image("/first.png"), image("/second.png")],
            )
            .unwrap();
        edb.append_follow_up_prompt_with_images(prompt, "", vec![image("/third.png")])
            .unwrap();
        let boundary = edb
            .append_tool_call(1, prompt, "compact", compact::TOOL_NAME, "{}")
            .unwrap();
        let history = snapshot(edb.events(), boundary).unwrap();
        assert!(history.contains("  User: \"exact <text>\"\n"));
        assert!(history.contains("  FollowUp: \"\"\n"));
        assert!(history.contains(
            "Attached image paths (not image content): [\"/first.png\",\"/second.png\"]"
        ));
        assert!(history.find("/second.png").unwrap() < history.find("/third.png").unwrap());
        assert!(!history.contains("data:image"));
    }

    #[test]
    fn context_clear_starts_a_new_history_lineage() {
        let mut edb = EventDataBase::new();
        edb.append_agent_kind_def(AgentKind::Interactive, "main-agent", None, None)
            .unwrap();
        edb.append_user_prompt("old request").unwrap();
        edb.append_context_cleared().unwrap();
        let current = edb.append_user_prompt("current request").unwrap();
        let boundary = edb
            .append_tool_call(1, current, "compact", compact::TOOL_NAME, "{}")
            .unwrap();
        let history = snapshot(edb.events(), boundary).unwrap();
        assert!(!history.contains("old request"));
        assert!(history.contains("current request"));
    }
}
