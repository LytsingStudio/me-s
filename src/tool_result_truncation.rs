use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use tiktoken_rs::o200k_base_singleton;
use unicode_segmentation::UnicodeSegmentation;

const METADATA_TOKEN_RESERVE: usize = 512;
const MAX_STALLED_ATTEMPTS: usize = 100;

pub fn truncate_for_model(tool_name: &str, value: Value, limit: usize) -> Value {
    truncate_for_model_with_limit(tool_name, value, limit)
}

fn truncate_for_model_with_limit(tool_name: &str, mut value: Value, limit: usize) -> Value {
    normalize_legacy_tool_result(tool_name, &mut value);
    let original = with_truncate_flag(value.clone(), false, None);
    if estimate_tokens(&original) <= limit {
        return original;
    }

    let mut state = TruncationState::new(tool_name, estimate_tokens(&original));
    if state.strategy == "unsupported" && !standard_error_is_croppable(&value) {
        return original;
    }
    if state.strategy == "unsupported" {
        state.strategy = "error_message";
    }
    let mut candidate = value;
    let mut previous_tokens = estimate_tokens(&candidate);
    let metadata_reserve = METADATA_TOKEN_RESERVE.min(limit / 8);
    let target = limit.saturating_sub(metadata_reserve);
    let mut stalled = 0;

    loop {
        let next = state.truncate_once(&candidate);
        let next_tokens = estimate_tokens(&next);
        if update_stalled_attempts(previous_tokens, next_tokens, &mut stalled) {
            return original;
        }
        candidate = next;
        previous_tokens = next_tokens;
        if next_tokens <= target && state.changed {
            let info = state.info(next_tokens);
            return with_truncate_flag(candidate, true, Some(info));
        }
    }
}

fn update_stalled_attempts(previous: usize, next: usize, stalled: &mut usize) -> bool {
    if next == previous {
        *stalled += 1;
    } else {
        *stalled = 0;
    }
    *stalled >= MAX_STALLED_ATTEMPTS
}

fn with_truncate_flag(mut value: Value, truncated: bool, info: Option<Value>) -> Value {
    let Some(object) = value.as_object_mut() else {
        return json!({
            "result": value,
            "truncate": truncated,
            "truncate_info": info,
        });
    };
    object.insert("truncate".into(), Value::Bool(truncated));
    object.remove("truncate_info");
    if let Some(info) = info {
        object.insert("truncate_info".into(), info);
    }
    value
}

fn estimate_tokens(value: &Value) -> usize {
    let encoded = serde_json::to_string(value).unwrap_or_default();
    o200k_base_singleton().encode_ordinary(&encoded).len()
}

#[derive(Default)]
struct TruncationState {
    tool_name: String,
    strategy: &'static str,
    original_tokens: usize,
    removed_units: usize,
    changed: bool,
    details: Map<String, Value>,
}

impl TruncationState {
    fn new(tool_name: &str, original_tokens: usize) -> Self {
        Self {
            tool_name: tool_name.into(),
            strategy: strategy_name(tool_name),
            original_tokens,
            ..Self::default()
        }
    }

    fn truncate_once(&mut self, value: &Value) -> Value {
        let mut next = value.clone();
        let changed = match self.tool_name.as_str() {
            "File.Read" => self.truncate_file_read(&mut next),
            "File.ReadBytes" => self.truncate_file_read_bytes(&mut next),
            "File.List" => self.truncate_detail_prefix(&mut next, "entries", "entries"),
            "File.Find" => self.truncate_detail_prefix(&mut next, "results", "results"),
            "File.Search" => self.truncate_file_search(&mut next),
            "File.Stat" => self.truncate_detail_prefix(&mut next, "entries", "entries"),
            "Terminal.Create" | "Terminal.Interact" => self.truncate_terminal(&mut next),
            "Terminal.List" => self.truncate_detail_prefix(&mut next, "sessions", "sessions"),
            "WebBrowser.Snapshot" => self.truncate_web_snapshot(&mut next),
            "WebBrowser.Pages" => self.truncate_pages(&mut next),
            "WebBrowser.RequireHumanAction" => self.truncate_human_action(&mut next),
            "WebBrowser.Click" => {
                self.truncate_detail_prefix(&mut next, "opened_page_ids", "opened_pages")
            }
            "Agent.Wait" => self.truncate_wait(&mut next),
            name if name.starts_with("WorkMap.") => self.truncate_workmap(&mut next),
            _ => false,
        } || self.truncate_error_or_updates(&mut next);
        if changed {
            self.changed = true;
        }
        next
    }

    fn info(&self, retained_tokens: usize) -> Value {
        let mut value = json!({
            "tool": self.tool_name,
            "strategy": self.strategy,
            "original_tokens": self.original_tokens,
            "retained_tokens": retained_tokens,
            "removed_units": self.removed_units,
        });
        if !self.details.is_empty() {
            value
                .as_object_mut()
                .expect("truncation info is an object")
                .insert("ranges".into(), Value::Object(self.details.clone()));
        }
        value
    }

    fn truncate_detail_prefix(&mut self, root: &mut Value, field: &str, key: &str) -> bool {
        let Some(array) = detail_mut(root)
            .and_then(Value::as_object_mut)
            .and_then(|object| object.get_mut(field))
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        let removed = remove_oldest_batch(array);
        if removed == 0 {
            return false;
        }
        self.record_prefix(key, removed);
        true
    }

    fn truncate_file_read(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        let Some(lines) = detail.get_mut("lines").and_then(Value::as_object_mut) else {
            return false;
        };
        let numbered = numbered_line_keys(lines);
        if let Some((left, right)) = middle_gap_keep_ends(numbered.len()) {
            let keys = numbered[left..right]
                .iter()
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>();
            for key in &keys {
                lines.remove(key);
            }
            self.removed_units += keys.len();
            self.details
                .insert("lines".into(), numbered_lines_range(lines));
            return true;
        }
        let candidate = numbered
            .iter()
            .filter_map(|(_, key)| {
                let value = lines.get(key)?;
                text_value_is_croppable(value).then(|| (key.clone(), json_size(value)))
            })
            .max_by_key(|(_, size)| *size)
            .map(|(key, _)| key);
        if let Some(key) = candidate
            && lines.get_mut(&key).is_some_and(crop_text_value)
        {
            self.removed_units += 1;
            self.details
                .insert("lines".into(), numbered_lines_range(lines));
            return true;
        }
        false
    }

    fn truncate_file_read_bytes(&mut self, root: &mut Value) -> bool {
        let previous_removed_end = self
            .details
            .get("bytes")
            .and_then(|range| range.get("removed_offset_end_exclusive"))
            .and_then(Value::as_u64);
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        let Some(data) = detail.get("data").and_then(Value::as_str) else {
            return false;
        };
        let Some(bytes) = hex_byte_tokens(data) else {
            return false;
        };
        if bytes.len() < 2 {
            return false;
        }
        let offset = detail.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let remove = batch_size(bytes.len()).min(bytes.len() - 1);
        let retained = bytes.len() - remove;
        let retained_data = bytes[..retained].join(" ");
        let removed_end = previous_removed_end.unwrap_or(offset + bytes.len() as u64);
        detail.insert("data".into(), Value::String(retained_data));
        detail.insert("length".into(), json!(retained));
        if let Some(size) = detail.get("size").and_then(Value::as_u64) {
            detail.insert("eof".into(), json!(offset + retained as u64 >= size));
        }
        self.removed_units += remove;
        self.details.insert(
            "bytes".into(),
            json!({
                "retained_offset_start": offset,
                "retained_offset_end_exclusive": offset + retained as u64,
                "removed_offset_start": offset + retained as u64,
                "removed_offset_end_exclusive": removed_end,
            }),
        );
        true
    }

    fn truncate_file_search(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        let Some(matches) = detail.get_mut("matches").and_then(Value::as_array_mut) else {
            return false;
        };
        if matches.len() > 1 {
            let removed = remove_oldest_batch(matches);
            self.record_prefix("matches", removed);
            return true;
        }
        let Some(item) = matches.first_mut().and_then(Value::as_object_mut) else {
            return false;
        };
        if let Some(lines) = item.get_mut("before").and_then(Value::as_object_mut)
            && !lines.is_empty()
        {
            let removed = remove_oldest_numbered_batch(lines);
            self.record_prefix("match_context_lines", removed);
            return true;
        }
        if let Some(lines) = item.get_mut("after").and_then(Value::as_object_mut)
            && !lines.is_empty()
        {
            let removed = remove_newest_numbered_batch(lines);
            self.removed_units += removed;
            self.details.insert(
                "match_context_lines".into(),
                json!({"removed_outer_count": self.removed_units}),
            );
            return true;
        }
        if crop_search_match_text(item) {
            self.removed_units += 1;
            let range = item
                .get("match_text")
                .and_then(Value::as_object)
                .and_then(|lines| numbered_line_keys(lines).first().cloned())
                .and_then(|(_, key)| {
                    item.get("match_text")
                        .and_then(Value::as_object)
                        .and_then(|lines| lines.get(&key))
                })
                .map(text_fragment_range)
                .unwrap_or_else(|| json!({}));
            self.details.insert("match_text".into(), range);
            return true;
        }
        false
    }

    fn truncate_terminal(&mut self, root: &mut Value) -> bool {
        let Some(updates) = root
            .as_object_mut()
            .and_then(|object| object.get_mut("terminal_updates"))
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        if updates.len() > 1 {
            let removed = remove_oldest_batch(updates);
            self.record_prefix("terminal_patches", removed);
            update_terminal_changed_rows(root);
            return true;
        }
        let Some(patch) = updates.first_mut().and_then(Value::as_object_mut) else {
            return false;
        };
        let Some(rows) = patch.get_mut("rows").and_then(Value::as_array_mut) else {
            return false;
        };
        if rows.is_empty() {
            return false;
        }
        let count = batch_size(rows.len());
        let removed_rows = rows.drain(..count).collect::<Vec<_>>();
        let row_numbers = removed_rows
            .iter()
            .filter_map(|row| row.get("terminal_row").and_then(Value::as_u64))
            .collect::<Vec<_>>();
        retain_referenced_terminal_styles(patch);
        self.removed_units += removed_rows.len();
        if let (Some(first), Some(last)) = (row_numbers.first(), row_numbers.last()) {
            merge_numeric_range(&mut self.details, "terminal_rows", *first, *last);
        }
        update_terminal_changed_rows(root);
        true
    }

    fn truncate_web_snapshot(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        let tree_size = detail
            .get("accessibility_tree")
            .map(json_size)
            .unwrap_or_default();
        let event_size = detail
            .get("browser_events")
            .map(json_size)
            .unwrap_or_default()
            + detail
                .get("dismissed_native_dialogs")
                .map(json_size)
                .unwrap_or_default();
        if tree_size >= event_size
            && let Some(tree) = detail.get_mut("accessibility_tree")
            && let Some(range) = crop_aria_value(tree)
        {
            self.removed_units += 1;
            merge_aria_range(&mut self.details, range);
            return true;
        }
        for field in ["browser_events", "dismissed_native_dialogs"] {
            if let Some(events) = detail.get_mut(field).and_then(Value::as_array_mut)
                && !events.is_empty()
            {
                let removed = remove_oldest_batch(events);
                self.record_prefix(field, removed);
                return true;
            }
        }
        if let Some(tree) = detail.get_mut("accessibility_tree")
            && let Some(range) = crop_aria_value(tree)
        {
            self.removed_units += 1;
            merge_aria_range(&mut self.details, range);
            return true;
        }
        false
    }

    fn truncate_human_action(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        let protected = [
            detail.get("page_id").and_then(Value::as_str),
            detail.get("active_page_id").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        let mut ordered = Vec::new();
        for field in ["changed_pages", "opened_pages", "closed_page_ids"] {
            if let Some(items) = detail.get(field).and_then(Value::as_array) {
                for item in items {
                    let id = if let Some(id) = item.as_str() {
                        Some(id)
                    } else {
                        item.get("page_id").and_then(Value::as_str)
                    };
                    if let Some(id) = id
                        && !protected.contains(id)
                        && !ordered.iter().any(|existing| existing == id)
                    {
                        ordered.push(id.to_owned());
                    }
                }
            }
        }
        if ordered.is_empty() {
            return false;
        }
        let remove = ordered
            .into_iter()
            .take(batch_size(detail_array_total(detail)))
            .collect::<BTreeSet<_>>();
        for field in ["changed_pages", "opened_pages", "closed_page_ids"] {
            if let Some(items) = detail.get_mut(field).and_then(Value::as_array_mut) {
                items.retain(|item| {
                    let id = item
                        .as_str()
                        .or_else(|| item.get("page_id").and_then(Value::as_str));
                    !id.is_some_and(|id| remove.contains(id))
                });
            }
        }
        self.removed_units += remove.len();
        self.details.insert(
            "page_change_transactions".into(),
            json!({"removed_count": self.removed_units}),
        );
        true
    }

    fn truncate_pages(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        let active = detail
            .get("active_page_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(pages) = detail.get_mut("pages").and_then(Value::as_array_mut) else {
            return false;
        };
        let removable = pages
            .iter()
            .enumerate()
            .filter(|(_, page)| page.get("page_id").and_then(Value::as_str) != active.as_deref())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if removable.is_empty() {
            return false;
        }
        let count = batch_size(removable.len());
        for index in removable.into_iter().take(count).rev() {
            pages.remove(index);
        }
        self.record_prefix("pages", count);
        true
    }

    fn truncate_wait(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        if let Some(progress) = detail.get_mut("progress").and_then(Value::as_array_mut)
            && !progress.is_empty()
        {
            let removed = remove_oldest_batch(progress);
            self.record_prefix("progress", removed);
            return true;
        }
        for field in ["final_answer", "error"] {
            if let Some(text) = detail.get_mut(field)
                && crop_text_value(text)
            {
                self.removed_units += 1;
                self.details.insert(field.into(), text_fragment_range(text));
                return true;
            }
        }
        false
    }

    fn truncate_workmap(&mut self, root: &mut Value) -> bool {
        let Some(detail) = detail_object_mut(root) else {
            return false;
        };
        if remove_oldest_workmap_note(detail) {
            self.record_prefix("notes", 1);
            return true;
        }
        if let Some(objectives) = detail.get_mut("objectives").and_then(Value::as_array_mut)
            && !objectives.is_empty()
        {
            let removed = remove_oldest_batch(objectives);
            self.record_prefix("history_objectives", removed);
            return true;
        }
        let protected_record_ids = workmap_record_reference_ids(detail);
        for field in ["current", "objective"] {
            if let Some(snapshot) = detail.get_mut(field)
                && remove_middle_workmap_plan(snapshot, &protected_record_ids)
            {
                self.removed_units += 1;
                self.details
                    .insert("plans".into(), json!({"removed_count": self.removed_units}));
                return true;
            }
        }
        if let Some(memory) = detail.get_mut("memory").and_then(Value::as_object_mut) {
            for field in ["facts", "agreements"] {
                if let Some(records) = memory.get_mut(field).and_then(Value::as_array_mut)
                    && records.len() > 2
                {
                    let removed = remove_middle_batch(records);
                    self.removed_units += removed;
                    self.details.insert(
                        "memory".into(),
                        json!({"removed_count": self.removed_units}),
                    );
                    return true;
                }
            }
        }
        if crop_largest_workmap_text(detail) {
            self.removed_units += 1;
            self.details.insert(
                "text_fields".into(),
                json!({"cropped_count": self.removed_units}),
            );
            return workmap_references_are_closed(detail);
        }
        false
    }

    fn truncate_error_or_updates(&mut self, root: &mut Value) -> bool {
        let update_field = root.as_object().and_then(|object| {
            if object.contains_key("updates") {
                Some("updates")
            } else if object.contains_key("other_updates") {
                Some("other_updates")
            } else {
                None
            }
        });
        if let Some(updates) = update_field
            .and_then(|field| root.as_object_mut()?.get_mut(field))
            .and_then(Value::as_array_mut)
            && !updates.is_empty()
        {
            let removed = remove_oldest_batch(updates);
            self.record_prefix("updates", removed);
            return true;
        }
        let Some(detail) = detail_mut(root) else {
            return false;
        };
        if let Some(message) = detail
            .as_object_mut()
            .and_then(|detail| detail.get_mut("error"))
            .and_then(Value::as_object_mut)
            .and_then(|error| error.get_mut("message"))
            && crop_text_value(message)
        {
            self.removed_units += 1;
            self.details
                .insert("error_message".into(), text_fragment_range(message));
            return true;
        }
        if detail.is_string() && crop_text_value(detail) {
            self.removed_units += 1;
            self.details
                .insert("error_detail".into(), text_fragment_range(detail));
            return true;
        }
        false
    }

    fn record_prefix(&mut self, key: &str, removed: usize) {
        self.removed_units += removed;
        let total = self
            .details
            .get(key)
            .and_then(|value| value.get("removed_prefix_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + removed as u64;
        self.details
            .insert(key.into(), json!({"removed_prefix_count": total}));
    }
}

fn standard_error_is_croppable(root: &Value) -> bool {
    root.get("result")
        .and_then(|result| result.get("detail"))
        .and_then(|detail| detail.get("error"))
        .and_then(|error| error.get("message"))
        .is_some_and(text_value_is_croppable)
}

fn strategy_name(tool_name: &str) -> &'static str {
    match tool_name {
        "File.Read" | "WebBrowser.Snapshot" => "middle",
        "File.ReadBytes" => "keep_prefix",
        "File.List"
        | "File.Find"
        | "File.Search"
        | "File.Stat"
        | "Terminal.Create"
        | "Terminal.Interact"
        | "Terminal.List"
        | "WebBrowser.Pages"
        | "WebBrowser.RequireHumanAction"
        | "WebBrowser.Click"
        | "Agent.Wait" => "drop_oldest",
        name if name.starts_with("WorkMap.") => "structure_aware",
        _ => "unsupported",
    }
}

fn detail_mut(root: &mut Value) -> Option<&mut Value> {
    root.as_object_mut()?
        .get_mut("result")?
        .as_object_mut()?
        .get_mut("detail")
}

fn detail_object_mut(root: &mut Value) -> Option<&mut Map<String, Value>> {
    detail_mut(root)?.as_object_mut()
}

fn normalize_legacy_tool_result(tool_name: &str, root: &mut Value) {
    let Some(detail) = detail_object_mut(root) else {
        return;
    };
    if tool_name == "File.Search" {
        if let Some(matches) = detail.get_mut("matches").and_then(Value::as_array_mut) {
            for matched in matches {
                let Some(matched) = matched.as_object_mut() else {
                    continue;
                };
                for field in ["before", "match_text", "after"] {
                    if let Some(lines) = matched.get_mut(field).and_then(Value::as_object_mut) {
                        normalize_numbered_logical_lines(lines);
                    }
                }
            }
        }
        return;
    }
    if tool_name != "File.Read" {
        return;
    }
    if detail.get("lines").is_some() {
        if let Some(lines) = detail.get_mut("lines").and_then(Value::as_object_mut) {
            normalize_numbered_logical_lines(lines);
        }
        return;
    }
    let Some(content) = detail
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let start_line = detail
        .get("start_line")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let split = split_file_lines(&content);
    let last_line = start_line.saturating_add(split.len().saturating_sub(1) as u64);
    let total_lines = detail
        .get("total_lines")
        .and_then(Value::as_u64)
        .unwrap_or(last_line)
        .max(last_line);
    let width = total_lines.to_string().len().max(1);
    let lines = split
        .into_iter()
        .enumerate()
        .map(|(offset, line)| {
            let number = start_line.saturating_add(offset as u64);
            (
                format!("{number:0width$}"),
                Value::String(without_line_ending(&line).to_owned()),
            )
        })
        .collect::<Map<_, _>>();
    detail.remove("content");
    detail.remove("content_segments");
    detail.insert("lines".into(), Value::Object(lines));
}

fn normalize_numbered_logical_lines(lines: &mut Map<String, Value>) {
    for value in lines.values_mut() {
        let Some(text) = value.as_str() else {
            continue;
        };
        let logical = without_line_ending(text);
        if logical.len() != text.len() {
            *value = Value::String(logical.to_owned());
        }
    }
}

fn without_line_ending(text: &str) -> &str {
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .or_else(|| text.strip_suffix('\r'))
        .unwrap_or(text)
}

fn batch_size(length: usize) -> usize {
    (length / 4).max(1).min(length)
}

fn middle_gap(length: usize) -> (usize, usize) {
    let remove = batch_size(length);
    let left = (length - remove) / 2;
    (left, left + remove)
}

fn middle_gap_keep_ends(length: usize) -> Option<(usize, usize)> {
    if length < 3 {
        return None;
    }
    let remove = batch_size(length).min(length - 2);
    let left = 1 + (length - remove - 2) / 2;
    Some((left, left + remove))
}

fn remove_oldest_batch(array: &mut Vec<Value>) -> usize {
    let count = batch_size(array.len());
    if count == 0 {
        return 0;
    }
    array.drain(..count);
    count
}

fn remove_middle_batch(array: &mut Vec<Value>) -> usize {
    if array.is_empty() {
        return 0;
    }
    let (left, right) = middle_gap(array.len());
    array.drain(left..right);
    right - left
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n').map(str::to_owned).collect()
}

fn split_file_lines(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let end = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => Some(index + 2),
            b'\r' | b'\n' => Some(index + 1),
            _ => None,
        };
        if let Some(end) = end {
            lines.push(text[start..end].to_owned());
            start = end;
            index = end;
        } else {
            index += 1;
        }
    }
    if start < bytes.len() {
        lines.push(text[start..].to_owned());
    }
    lines
}

fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

fn text_fragment(text: &str) -> Option<Value> {
    let boundaries = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let count = boundaries.len().saturating_sub(1);
    if count < 3 {
        return None;
    }
    let (left, right) = middle_gap_keep_ends(count)?;
    let left_byte = boundaries[left];
    let right_byte = boundaries[right];
    Some(json!({
        "kind": "text_fragments",
        "original_bytes": text.len(),
        "fragments": [
            {"start_byte": 0, "end_byte": left_byte, "text": &text[..left_byte]},
            {"start_byte": right_byte, "end_byte": text.len(), "text": &text[right_byte..]},
        ]
    }))
}

fn crop_search_match_text(item: &mut Map<String, Value>) -> bool {
    let column = item.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
    let match_length = item
        .get("match_length")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let Some(match_text) = item.get_mut("match_text").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some((_, key)) = numbered_line_keys(match_text).first().cloned() else {
        return false;
    };
    if match_text.get(&key).is_some_and(Value::is_object) {
        return match_text.get_mut(&key).is_some_and(crop_text_value);
    }
    let Some(text) = match_text
        .get(&key)
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return false;
    };
    let char_boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let char_count = char_boundaries.len().saturating_sub(1);
    if char_count < 5 {
        return false;
    }
    let match_start_char = column.saturating_sub(1).min(char_count);
    let match_end_char = match_start_char
        .saturating_add(match_length)
        .min(char_count);
    let edge = (char_count / 8).max(1);
    let context = 16.min(char_count);
    let match_edge = 64;
    let mut ranges = vec![
        (0, edge.min(char_count)),
        (
            match_start_char.saturating_sub(context),
            match_start_char
                .saturating_add(match_edge)
                .min(match_end_char),
        ),
        (
            match_end_char
                .saturating_sub(match_edge)
                .max(match_start_char),
            match_end_char.saturating_add(context).min(char_count),
        ),
        (char_count.saturating_sub(edge), char_count),
    ];
    ranges.sort_unstable();
    let mut merged = Vec::<(usize, usize)>::new();
    for (start, end) in ranges {
        if start >= end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    if merged.len() == 1 && merged[0] == (0, char_count) {
        return false;
    }
    let fragments = merged
        .into_iter()
        .map(|(start, end)| {
            let start_byte = char_boundaries[start];
            let end_byte = char_boundaries[end];
            json!({
                "start_byte": start_byte,
                "end_byte": end_byte,
                "text": &text[start_byte..end_byte],
            })
        })
        .collect::<Vec<_>>();
    match_text.insert(
        key,
        json!({"kind":"text_fragments","original_bytes":text.len(),"fragments":fragments}),
    );
    true
}

fn crop_text_value(value: &mut Value) -> bool {
    if let Some(text) = value.as_str() {
        let Some(fragmented) = text_fragment(text) else {
            return false;
        };
        *value = fragmented;
        return true;
    }
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    if object.get("kind").and_then(Value::as_str) != Some("text_fragments") {
        return false;
    }
    let Some(fragments) = object.get_mut("fragments").and_then(Value::as_array_mut) else {
        return false;
    };
    if fragments.is_empty() {
        return false;
    }
    let total = fragments
        .iter()
        .filter_map(|fragment| fragment.get("text").and_then(Value::as_str))
        .map(|text| text.graphemes(true).count())
        .sum::<usize>();
    if total <= 2 {
        return false;
    }
    let mut remaining = batch_size(total);
    if let Some(first) = fragments.first_mut().and_then(Value::as_object_mut) {
        let remove = remaining.div_ceil(2);
        remaining -= shrink_fragment_inner(first, remove, false);
    }
    if remaining > 0
        && let Some(last) = fragments.last_mut().and_then(Value::as_object_mut)
    {
        shrink_fragment_inner(last, remaining, true);
    }
    fragments.retain(|fragment| {
        fragment
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    });
    true
}

fn shrink_fragment_inner(
    fragment: &mut Map<String, Value>,
    amount: usize,
    from_start: bool,
) -> usize {
    let Some(text) = fragment
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return 0;
    };
    let boundaries = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let count = boundaries.len().saturating_sub(1);
    let remove = amount.min(count.saturating_sub(1));
    if remove == 0 {
        return 0;
    }
    if from_start {
        let byte = boundaries[remove];
        let start = fragment
            .get("start_byte")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        fragment.insert("start_byte".into(), json!(start + byte as u64));
        fragment.insert("text".into(), Value::String(text[byte..].to_owned()));
    } else {
        let byte = boundaries[count - remove];
        let start = fragment
            .get("start_byte")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        fragment.insert("end_byte".into(), json!(start + byte as u64));
        fragment.insert("text".into(), Value::String(text[..byte].to_owned()));
    }
    remove
}

fn text_fragment_range(value: &Value) -> Value {
    let fragments = value
        .get("fragments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let first_end = fragments
        .first()
        .and_then(|fragment| fragment.get("end_byte"))
        .and_then(Value::as_u64);
    let last_start = fragments
        .last()
        .and_then(|fragment| fragment.get("start_byte"))
        .and_then(Value::as_u64);
    json!({"removed_byte_start": first_end, "removed_byte_end": last_start})
}

fn numbered_line_keys(lines: &Map<String, Value>) -> Vec<(u64, String)> {
    let mut numbered = lines
        .keys()
        .filter_map(|key| key.parse::<u64>().ok().map(|number| (number, key.clone())))
        .collect::<Vec<_>>();
    numbered.sort_by_key(|(number, _)| *number);
    numbered
}

fn remove_oldest_numbered_batch(lines: &mut Map<String, Value>) -> usize {
    let numbered = numbered_line_keys(lines);
    let count = batch_size(numbered.len());
    for (_, key) in numbered.into_iter().take(count) {
        lines.remove(&key);
    }
    count
}

fn remove_newest_numbered_batch(lines: &mut Map<String, Value>) -> usize {
    let numbered = numbered_line_keys(lines);
    let count = batch_size(numbered.len());
    let keep = numbered.len().saturating_sub(count);
    for (_, key) in numbered.into_iter().skip(keep) {
        lines.remove(&key);
    }
    count
}

fn numbered_lines_range(lines: &Map<String, Value>) -> Value {
    let numbered = numbered_line_keys(lines);
    let removed_line_ranges = numbered
        .windows(2)
        .filter_map(|pair| {
            let start = pair[0].0.checked_add(1)?;
            let end = pair[1].0.checked_sub(1)?;
            (start <= end).then(|| json!({"start_line": start, "end_line": end}))
        })
        .collect::<Vec<_>>();
    let cropped_lines = numbered
        .iter()
        .filter_map(|(number, key)| {
            let value = lines.get(key)?;
            value.is_object().then(|| {
                json!({
                    "line": number,
                    "removed_bytes": text_fragment_range(value),
                })
            })
        })
        .collect::<Vec<_>>();
    json!({
        "retained_first_line": numbered.first().map(|(number, _)| number),
        "retained_last_line": numbered.last().map(|(number, _)| number),
        "removed_line_ranges": removed_line_ranges,
        "cropped_lines": cropped_lines,
    })
}

fn hex_byte_tokens(data: &str) -> Option<Vec<&str>> {
    if data.is_empty() {
        return Some(Vec::new());
    }
    let bytes = data.split(' ').collect::<Vec<_>>();
    bytes
        .iter()
        .all(|byte| {
            byte.len() == 2
                && byte
                    .bytes()
                    .all(|digit| digit.is_ascii_digit() || (b'a'..=b'f').contains(&digit))
        })
        .then_some(bytes)
}

fn retain_referenced_terminal_styles(patch: &mut Map<String, Value>) {
    let used = patch
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|row| {
            row.get("style_spans")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|span| span.get("style").and_then(Value::as_u64))
        .collect::<BTreeSet<_>>();
    if let Some(styles) = patch.get_mut("styles").and_then(Value::as_array_mut) {
        styles.retain(|style| {
            style
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|id| used.contains(&id))
        });
    }
}

fn update_terminal_changed_rows(root: &mut Value) {
    let count = root
        .get("terminal_updates")
        .and_then(Value::as_array)
        .and_then(|updates| updates.last())
        .and_then(|patch| patch.get("rows"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if let Some(detail) = detail_object_mut(root) {
        detail.insert("changed_rows".into(), json!(count));
    }
}

fn merge_numeric_range(details: &mut Map<String, Value>, key: &str, first: u64, last: u64) {
    let old_first = details
        .get(key)
        .and_then(|value| value.get("first"))
        .and_then(Value::as_u64)
        .unwrap_or(first);
    let old_last = details
        .get(key)
        .and_then(|value| value.get("last"))
        .and_then(Value::as_u64)
        .unwrap_or(last);
    details.insert(
        key.into(),
        json!({"first": old_first.min(first), "last": old_last.max(last)}),
    );
}

fn crop_aria_value(value: &mut Value) -> Option<Value> {
    if value
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "aria_fragments")
    {
        return crop_aria_fragments(value);
    }
    if value.is_object() {
        let changed = crop_text_value(value);
        return changed.then(|| text_fragment_range(value));
    }
    let text = value.as_str()?.to_owned();
    let lines = split_lines(&text);
    if lines.len() >= 3
        && let Some((start, end)) = aria_middle_subtree_range(&lines)
    {
        let mut fragments = Vec::new();
        push_aria_fragment(&mut fragments, 1, &lines[..start]);
        push_aria_fragment(&mut fragments, end as u64 + 1, &lines[end..]);
        *value = json!({
            "kind": "aria_fragments",
            "original_lines": lines.len(),
            "fragments": fragments,
        });
        return Some(json!({
            "removed_line_start": start + 1,
            "removed_line_end": end,
        }));
    }
    let changed = crop_text_value(value);
    changed.then(|| text_fragment_range(value))
}

fn crop_aria_fragments(value: &mut Value) -> Option<Value> {
    let fragments = value.get_mut("fragments")?.as_array_mut()?;
    let last_index = fragments.len().checked_sub(1)?;
    let index = fragments
        .iter()
        .enumerate()
        .filter_map(|(index, fragment)| {
            let text = fragment.get("text")?;
            text_value_is_croppable(text).then(|| (index, json_size(text)))
        })
        .max_by_key(|(_, length)| *length)?
        .0;
    let fragment = fragments.get(index)?.as_object()?;
    let start_line = fragment.get("start_line")?.as_u64()?;
    if fragment.get("text")?.is_object() {
        let text = fragments[index].get_mut("text")?;
        let changed = crop_text_value(text);
        return changed.then(|| {
            json!({
                "fragment_start_line": start_line,
                "removed_bytes": text_fragment_range(text),
            })
        });
    }
    let text = fragment.get("text")?.as_str()?.to_owned();
    let lines = split_lines(&text);
    let from_start = index == last_index;
    if lines.len() >= 2
        && let Some((start, end)) = aria_inner_edge_subtree_range(&lines, from_start)
    {
        let retained = lines[..start]
            .iter()
            .chain(lines[end..].iter())
            .cloned()
            .collect::<String>();
        let fragment = fragments[index].as_object_mut()?;
        fragment.insert("text".into(), Value::String(retained));
        if from_start {
            fragment.insert("start_line".into(), json!(start_line + end as u64));
        } else {
            fragment.insert("end_line".into(), json!(start_line + start as u64 - 1));
        }
        return Some(json!({
            "removed_line_start": start_line + start as u64,
            "removed_line_end": start_line + end as u64 - 1,
        }));
    }
    let text = fragments[index].get_mut("text")?;
    let changed = crop_text_value(text);
    changed.then(|| {
        json!({
            "fragment_start_line": start_line,
            "removed_bytes": text_fragment_range(text),
        })
    })
}

fn text_value_is_croppable(value: &Value) -> bool {
    if let Some(text) = value.as_str() {
        return text.graphemes(true).count() >= 3;
    }
    value
        .get("fragments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|fragment| fragment.get("text").and_then(Value::as_str))
        .map(|text| text.graphemes(true).count())
        .sum::<usize>()
        >= 3
}

fn push_aria_fragment(output: &mut Vec<Value>, start_line: u64, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    output.push(json!({
        "start_line": start_line,
        "end_line": start_line + lines.len() as u64 - 1,
        "text": lines.concat(),
    }));
}

fn merge_aria_range(details: &mut Map<String, Value>, range: Value) {
    let key = "accessibility_tree";
    let Some(first) = range.get("removed_line_start").and_then(Value::as_u64) else {
        details.insert(key.into(), range);
        return;
    };
    let Some(last) = range.get("removed_line_end").and_then(Value::as_u64) else {
        details.insert(key.into(), range);
        return;
    };
    let old_first = details
        .get(key)
        .and_then(|value| value.get("removed_line_start"))
        .and_then(Value::as_u64)
        .unwrap_or(first);
    let old_last = details
        .get(key)
        .and_then(|value| value.get("removed_line_end"))
        .and_then(Value::as_u64)
        .unwrap_or(last);
    details.insert(
        key.into(),
        json!({
            "removed_line_start": old_first.min(first),
            "removed_line_end": old_last.max(last),
        }),
    );
}

fn aria_middle_subtree_range(lines: &[String]) -> Option<(usize, usize)> {
    let indents = lines
        .iter()
        .map(|line| {
            line.chars()
                .take_while(|character| character.is_whitespace())
                .count()
        })
        .collect::<Vec<_>>();
    let mut groups = BTreeMap::<Option<usize>, Vec<usize>>::new();
    let mut stack = Vec::<usize>::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        while stack
            .last()
            .is_some_and(|parent| indents[*parent] >= indents[index])
        {
            stack.pop();
        }
        groups.entry(stack.last().copied()).or_default().push(index);
        stack.push(index);
    }
    let siblings = groups
        .values()
        .filter(|children| children.len() >= 3)
        .max_by_key(|children| {
            children
                .iter()
                .map(|index| lines[*index].len())
                .sum::<usize>()
        })?;
    let remove = batch_size(siblings.len()).min(siblings.len().saturating_sub(2));
    if remove == 0 {
        return None;
    }
    let left = (siblings.len() - remove) / 2;
    let start = siblings[left];
    let last_root = siblings[left + remove - 1];
    let last_indent = indents[last_root];
    let mut end = last_root + 1;
    while end < lines.len() && (lines[end].trim().is_empty() || indents[end] > last_indent) {
        end += 1;
    }
    Some((start, end))
}

fn aria_inner_edge_subtree_range(lines: &[String], from_start: bool) -> Option<(usize, usize)> {
    let indents = lines
        .iter()
        .map(|line| {
            line.chars()
                .take_while(|character| character.is_whitespace())
                .count()
        })
        .collect::<Vec<_>>();
    let mut groups = BTreeMap::<Option<usize>, Vec<usize>>::new();
    let mut stack = Vec::<usize>::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        while stack
            .last()
            .is_some_and(|parent| indents[*parent] >= indents[index])
        {
            stack.pop();
        }
        groups.entry(stack.last().copied()).or_default().push(index);
        stack.push(index);
    }
    groups
        .values()
        .filter(|siblings| siblings.len() >= 2)
        .filter_map(|siblings| {
            let remove = batch_size(siblings.len()).min(siblings.len() - 1);
            let chosen = if from_start {
                &siblings[..remove]
            } else {
                &siblings[siblings.len() - remove..]
            };
            let start = *chosen.first()?;
            let last_root = *chosen.last()?;
            let last_indent = indents[last_root];
            let mut end = last_root + 1;
            while end < lines.len() && (lines[end].trim().is_empty() || indents[end] > last_indent)
            {
                end += 1;
            }
            let touches_inner_edge = if from_start {
                start == 0
            } else {
                end == lines.len()
            };
            touches_inner_edge.then_some((start, end))
        })
        .max_by_key(|(start, end)| end - start)
}

fn detail_array_total(detail: &Map<String, Value>) -> usize {
    ["changed_pages", "opened_pages", "closed_page_ids"]
        .into_iter()
        .filter_map(|field| detail.get(field).and_then(Value::as_array))
        .map(Vec::len)
        .sum::<usize>()
        .max(1)
}

fn remove_oldest_workmap_note(detail: &mut Map<String, Value>) -> bool {
    for field in ["current", "objective"] {
        let Some(plans) = detail
            .get_mut(field)
            .and_then(Value::as_object_mut)
            .and_then(|snapshot| snapshot.get_mut("plans"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let candidate = plans
            .iter()
            .enumerate()
            .filter_map(|(plan_index, plan)| {
                let note = plan.get("notes")?.as_array()?.first()?;
                let timestamp = note
                    .get("created_at_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Some((timestamp, plan_index))
            })
            .min();
        if let Some((_, plan_index)) = candidate
            && let Some(notes) = plans[plan_index]
                .get_mut("notes")
                .and_then(Value::as_array_mut)
        {
            notes.remove(0);
            return true;
        }
    }
    false
}

fn remove_middle_workmap_plan(snapshot: &mut Value, protected_ids: &BTreeSet<String>) -> bool {
    let Some(plans) = snapshot
        .as_object_mut()
        .and_then(|snapshot| snapshot.get_mut("plans"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    if plans.len() <= 2 {
        return false;
    }
    let middle = plans.len() / 2;
    let candidate = (0..plans.len())
        .filter(|index| {
            let plan = &plans[*index];
            plan.get("plan")
                .and_then(|plan| plan.get("state"))
                .and_then(Value::as_str)
                != Some("active")
                && plan
                    .get("plan")
                    .and_then(|plan| plan.get("id"))
                    .and_then(Value::as_str)
                    .is_none_or(|id| !protected_ids.contains(id))
        })
        .min_by_key(|index| index.abs_diff(middle));
    if let Some(index) = candidate {
        plans.remove(index);
        true
    } else {
        false
    }
}

fn crop_largest_workmap_text(detail: &mut Map<String, Value>) -> bool {
    let mut root = Value::Object(std::mem::take(detail));
    let mut paths = Vec::<Vec<PathPart>>::new();
    collect_workmap_text_paths(&root, &mut Vec::new(), &mut paths, true);
    let mut changed = crop_first_available_workmap_text(&mut root, paths);
    if !changed {
        let mut record_paths = Vec::<Vec<PathPart>>::new();
        if let Some(records) = root.get("records") {
            let mut prefix = vec![PathPart::Key("records".into())];
            collect_workmap_text_paths(records, &mut prefix, &mut record_paths, false);
        }
        changed = crop_first_available_workmap_text(&mut root, record_paths);
    }
    *detail = root.as_object_mut().map(std::mem::take).unwrap_or_default();
    changed
}

fn crop_first_available_workmap_text(root: &mut Value, mut paths: Vec<Vec<PathPart>>) -> bool {
    paths.sort_by_key(|path| std::cmp::Reverse(path_string_len(root, path)));
    paths
        .into_iter()
        .any(|path| value_at_path_mut(root, &path).is_some_and(crop_text_value))
}

#[derive(Clone)]
enum PathPart {
    Key(String),
    Index(usize),
}

fn collect_workmap_text_paths(
    value: &Value,
    current: &mut Vec<PathPart>,
    output: &mut Vec<Vec<PathPart>>,
    skip_records: bool,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if skip_records && key == "records" {
                    continue;
                }
                current.push(PathPart::Key(key.clone()));
                if matches!(
                    key.as_str(),
                    "content" | "description" | "outcome" | "verification" | "status_reason"
                ) && (child.is_string()
                    || child.get("kind").and_then(Value::as_str) == Some("text_fragments"))
                {
                    output.push(current.clone());
                } else {
                    collect_workmap_text_paths(child, current, output, skip_records);
                }
                current.pop();
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                current.push(PathPart::Index(index));
                collect_workmap_text_paths(child, current, output, skip_records);
                current.pop();
            }
        }
        _ => {}
    }
}

fn workmap_record_reference_ids(detail: &Map<String, Value>) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let Some(records) = detail.get("records").and_then(Value::as_array) else {
        return ids;
    };
    for record in records {
        collect_reference_ids(record, &mut ids);
    }
    ids
}

fn collect_reference_ids(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if matches!(
                    key.as_str(),
                    "id" | "objective_id" | "plan_id" | "memory_id" | "replacement_id"
                ) && let Some(id) = child.as_str()
                {
                    ids.insert(id.to_owned());
                }
                collect_reference_ids(child, ids);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_reference_ids(child, ids);
            }
        }
        _ => {}
    }
}

fn path_string_len(root: &Value, path: &[PathPart]) -> usize {
    value_at_path(root, path).map_or(0, json_size)
}

fn value_at_path<'a>(mut value: &'a Value, path: &[PathPart]) -> Option<&'a Value> {
    for part in path {
        value = match part {
            PathPart::Key(key) => value.get(key)?,
            PathPart::Index(index) => value.get(*index)?,
        };
    }
    Some(value)
}

fn value_at_path_mut<'a>(mut value: &'a mut Value, path: &[PathPart]) -> Option<&'a mut Value> {
    for part in path {
        value = match part {
            PathPart::Key(key) => value.get_mut(key)?,
            PathPart::Index(index) => value.get_mut(*index)?,
        };
    }
    Some(value)
}

fn workmap_references_are_closed(detail: &Map<String, Value>) -> bool {
    for field in ["current", "objective"] {
        let Some(snapshot) = detail.get(field).and_then(Value::as_object) else {
            continue;
        };
        let objective_id = snapshot
            .get("objective")
            .and_then(|objective| objective.get("id"))
            .and_then(Value::as_str);
        let Some(plans) = snapshot.get("plans").and_then(Value::as_array) else {
            continue;
        };
        for plan in plans {
            let Some(plan_record) = plan.get("plan") else {
                return false;
            };
            if objective_id.is_some()
                && plan_record.get("objective_id").and_then(Value::as_str) != objective_id
            {
                return false;
            }
            let plan_id = plan_record.get("id").and_then(Value::as_str);
            if plan
                .get("notes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|note| note.get("plan_id").and_then(Value::as_str) != plan_id)
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapper(detail: Value) -> Value {
        json!({"result":{"state":"succeeded","exit_code":null,"detail":detail}})
    }

    fn numbered_lines(lines: impl IntoIterator<Item = String>, first_line: u64) -> Value {
        Value::Object(
            lines
                .into_iter()
                .enumerate()
                .map(|(offset, line)| ((first_line + offset as u64).to_string(), json!(line)))
                .collect(),
        )
    }

    #[test]
    fn small_result_is_explicitly_not_truncated() {
        let result =
            truncate_for_model_with_limit("File.List", wrapper(json!({"entries":[]})), 100);
        assert_eq!(result["truncate"], false);
        assert!(result.get("truncate_info").is_none());
    }

    #[test]
    fn legacy_file_read_is_projected_as_numbered_lines_without_changing_the_source() {
        let original = wrapper(json!({
            "path":"old.txt",
            "content":"first\r\n\rthird",
            "start_line":1,
            "end_line":3,
            "total_lines":12,
            "hash":"1234abcd"
        }));
        let result = truncate_for_model_with_limit("File.Read", original.clone(), 1_000);
        assert_eq!(result["truncate"], false);
        assert!(result["result"]["detail"].get("content").is_none());
        assert_eq!(
            result["result"]["detail"]["lines"],
            json!({"01":"first", "02":"", "03":"third"})
        );
        assert_eq!(original["result"]["detail"]["content"], "first\r\n\rthird");
        assert!(original["result"]["detail"].get("lines").is_none());
    }

    #[test]
    fn legacy_file_search_lines_are_projected_without_line_endings() {
        let original = wrapper(json!({
            "path":"src",
            "matches":[{
                "path":"src/main.rs",
                "before":{"01":"before\r\n"},
                "match_text":{"02":"needle\n"},
                "after":{"03":"\r"},
                "column":1,
                "match_length":6
            }],
            "truncated":false
        }));
        let result = truncate_for_model_with_limit("File.Search", original.clone(), 1_000);
        assert_eq!(result["truncate"], false);
        assert_eq!(
            result["result"]["detail"]["matches"][0]["before"],
            json!({"01":"before"})
        );
        assert_eq!(
            result["result"]["detail"]["matches"][0]["match_text"],
            json!({"02":"needle"})
        );
        assert_eq!(
            result["result"]["detail"]["matches"][0]["after"],
            json!({"03":""})
        );
        assert_eq!(
            original["result"]["detail"]["matches"][0]["match_text"]["02"],
            "needle\n"
        );
    }

    #[test]
    fn file_read_keeps_both_ends_as_numbered_lines() {
        let lines = (1..=200)
            .map(|line| format!("line-{line:04} {}\n", "x".repeat(40)))
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "File.Read",
            wrapper(
                json!({"path":"a.txt","lines":numbered_lines(lines, 1),"start_line":1,"end_line":200,"total_lines":200}),
            ),
            500,
        );
        assert_eq!(result["truncate"], true);
        let retained = result["result"]["detail"]["lines"].as_object().unwrap();
        assert!(retained.contains_key("1"));
        assert!(retained.contains_key("200"));
        assert!(retained.len() < 200);
        assert!(!retained.contains_key("100"));
        assert_eq!(result["result"]["detail"]["total_lines"], 200);
        assert!(
            result["truncate_info"]["ranges"]["lines"]["removed_line_ranges"]
                .as_array()
                .is_some_and(|ranges| !ranges.is_empty())
        );
    }

    #[test]
    fn read_bytes_keeps_only_complete_prefix_bytes() {
        let bytes = (0..=255).cycle().take(20_000).collect::<Vec<u8>>();
        let data = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let result = truncate_for_model_with_limit(
            "File.ReadBytes",
            wrapper(json!({"data":data,"offset":10,"length":bytes.len(),"size":20_010,"eof":true})),
            600,
        );
        assert_eq!(result["truncate"], true);
        assert_eq!(result["truncate_info"]["strategy"], "keep_prefix");
        let retained = result["result"]["detail"]["data"]
            .as_str()
            .unwrap()
            .split(' ')
            .collect::<Vec<_>>();
        assert_eq!(
            retained.len() as u64,
            result["result"]["detail"]["length"].as_u64().unwrap()
        );
        assert!(retained.len() < bytes.len());
        assert_eq!(retained.first().copied(), Some("00"));
        assert_eq!(retained.get(1).copied(), Some("01"));
        for (actual, expected) in retained.iter().zip(&bytes) {
            assert_eq!(*actual, format!("{expected:02x}"));
        }
        assert!(result["result"]["detail"].get("base64").is_none());
        assert!(result["result"]["detail"].get("chunks").is_none());
        assert_eq!(result["result"]["detail"]["offset"], 10);
        assert_eq!(
            result["truncate_info"]["ranges"]["bytes"]["retained_offset_end_exclusive"],
            10 + retained.len() as u64
        );
        assert_eq!(
            result["truncate_info"]["ranges"]["bytes"]["removed_offset_start"],
            10 + retained.len() as u64
        );
        assert_eq!(
            result["truncate_info"]["ranges"]["bytes"]["removed_offset_end_exclusive"],
            20_010
        );
        assert_eq!(result["result"]["detail"]["eof"], false);
    }

    #[test]
    fn single_line_unicode_read_keeps_exact_byte_fragments_and_original_is_unchanged() {
        let content = format!("开头{}结尾", "中间🙂".repeat(8_000));
        let original = wrapper(json!({
            "path":"unicode.txt","lines":{"1":content},"start_line":1,"end_line":1,
            "total_lines":1,"hash":"1234abcd","encoding":"utf-8"
        }));
        let result = truncate_for_model_with_limit("File.Read", original.clone(), 1_500);
        assert_eq!(result["truncate"], true);
        assert_eq!(original["result"]["detail"]["lines"]["1"], content);
        let value = &result["result"]["detail"]["lines"]["1"];
        assert_eq!(value["kind"], "text_fragments");
        let fragments = value["fragments"].as_array().unwrap();
        assert!(
            fragments.first().unwrap()["text"]
                .as_str()
                .unwrap()
                .starts_with("开头")
        );
        assert!(
            fragments.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .ends_with("结尾")
        );
        for fragment in fragments {
            let start = fragment["start_byte"].as_u64().unwrap() as usize;
            let end = fragment["end_byte"].as_u64().unwrap() as usize;
            assert_eq!(fragment["text"], content[start..end]);
        }
    }

    #[test]
    fn file_read_continues_cropping_when_both_boundary_lines_are_huge() {
        let first = format!("FIRST{}\n", "甲".repeat(12_000));
        let last = format!("LAST{}\n", "乙".repeat(12_000));
        let result = truncate_for_model_with_limit(
            "File.Read",
            wrapper(
                json!({"lines":{"1":first,"2":last},"start_line":1,"end_line":2,"total_lines":2}),
            ),
            1_500,
        );
        assert_eq!(result["truncate"], true);
        let first_fragments = result["result"]["detail"]["lines"]["1"]["fragments"]
            .as_array()
            .unwrap();
        assert!(
            first_fragments.first().unwrap()["text"]
                .as_str()
                .unwrap()
                .starts_with("FIRST")
        );
        let last_fragments = result["result"]["detail"]["lines"]["2"]["fragments"]
            .as_array()
            .unwrap();
        assert!(
            last_fragments.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .ends_with('乙')
        );
    }

    #[test]
    fn prefix_lists_drop_oldest_complete_objects() {
        let entries = (0..500)
            .map(|index| json!({"path":format!("{index:04}.txt"),"description":"x".repeat(80)}))
            .collect::<Vec<_>>();
        let result =
            truncate_for_model_with_limit("File.List", wrapper(json!({"entries":entries})), 700);
        assert_eq!(result["truncate"], true);
        let retained = result["result"]["detail"]["entries"].as_array().unwrap();
        assert!(retained.first().unwrap()["path"].as_str().unwrap() > "0000.txt");
    }

    #[test]
    fn terminal_crop_rebuilds_exact_style_definitions() {
        let rows = (0..400)
            .map(|row| json!({"terminal_row":row,"text":"x".repeat(100),"style_spans":[{"start_column":0,"width":1,"style":if row < 200 {1} else {2}}]}))
            .collect::<Vec<_>>();
        let value = json!({
            "terminal_updates":[{"type":"terminal_patch","sequence":1,"styles":[{"id":1,"foreground":"#ff0000"},{"id":2,"foreground":"#00ff00"},{"id":3,"foreground":"#0000ff"}],"rows":rows}],
            "result":{"state":"succeeded","exit_code":null,"detail":{"changed_rows":400}}
        });
        let result = truncate_for_model_with_limit("Terminal.Interact", value, 800);
        assert_eq!(result["truncate"], true);
        let patch = &result["terminal_updates"][0];
        let used = patch["rows"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row["style_spans"].as_array().unwrap())
            .map(|span| span["style"].as_u64().unwrap())
            .collect::<BTreeSet<_>>();
        let defined = patch["styles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|style| style["id"].as_u64().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(used, defined);
    }

    #[test]
    fn agent_wait_drops_oldest_progress_and_keeps_terminal_state() {
        let progress = (0..200)
            .map(|index| json!({"assistant_text":format!("step {index} {}", "x".repeat(100)),"tool_calls":["File.Read"]}))
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "Agent.Wait",
            wrapper(
                json!({"session_id":"agent-1","state":"working","turn_id":4,"final_answer":null,"progress":progress,"context_usage":null,"compact_count":0}),
            ),
            700,
        );
        assert_eq!(result["truncate"], true);
        assert_eq!(result["result"]["detail"]["state"], "working");
        let retained = result["result"]["detail"]["progress"].as_array().unwrap();
        assert!(
            retained.first().unwrap()["assistant_text"]
                .as_str()
                .unwrap()
                .starts_with("step 1")
        );
    }

    #[test]
    fn unknown_oversized_tool_returns_original_with_false_flag() {
        let original = wrapper(json!({"content":"x".repeat(100_000)}));
        let result = truncate_for_model_with_limit("Unknown.Tool", original.clone(), 100);
        assert_eq!(result["truncate"], false);
        assert_eq!(result["result"], original["result"]);
    }

    #[test]
    fn every_tool_can_safely_crop_the_standard_error_message() {
        let original = wrapper(json!({
            "error": {
                "code": "operation_failed",
                "message": format!("ERROR-BEGIN{}ERROR-END", "详情".repeat(20_000)),
                "retryable": false
            }
        }));
        let result = truncate_for_model_with_limit("Unknown.Tool", original, 900);
        assert_eq!(result["truncate"], true);
        assert_eq!(result["truncate_info"]["strategy"], "error_message");
        let error = &result["result"]["detail"]["error"];
        assert_eq!(error["code"], "operation_failed");
        assert_eq!(error["retryable"], false);
        assert_eq!(error["message"]["kind"], "text_fragments");
        let fragments = error["message"]["fragments"].as_array().unwrap();
        assert!(
            fragments.first().unwrap()["text"]
                .as_str()
                .unwrap()
                .starts_with("ERROR-BEGIN")
        );
        assert!(
            fragments.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .ends_with("ERROR-END")
        );
    }

    #[test]
    fn token_estimation_treats_model_special_markers_as_ordinary_tool_text() {
        let marker_text = "<|endoftext|>".repeat(2_000);
        let value = wrapper(json!({
            "lines": {"1": marker_text},
            "start_line": 1,
            "end_line": 1,
            "total_lines": 1
        }));
        let encoded =
            serde_json::to_string(&with_truncate_flag(value.clone(), false, None)).unwrap();
        let bpe = o200k_base_singleton();
        let ordinary_tokens = bpe.encode_ordinary(&encoded).len();
        let special_tokens = bpe.encode_with_special_tokens(&encoded).len();
        assert!(ordinary_tokens > special_tokens);

        let limit = (ordinary_tokens + special_tokens) / 2;
        let result = truncate_for_model_with_limit("File.Read", value, limit);
        assert_eq!(result["truncate"], true);
    }

    #[test]
    fn aria_crop_preserves_valid_sibling_indentation() {
        let tree = format!(
            "- document:\n{}",
            (0..100)
                .map(|index| format!("  - paragraph \"item {index} {}\"\n", "x".repeat(80)))
                .collect::<String>()
        );
        let result = truncate_for_model_with_limit(
            "WebBrowser.Snapshot",
            wrapper(
                json!({"accessibility_tree":tree,"browser_events":[],"dismissed_native_dialogs":[]}),
            ),
            700,
        );
        assert_eq!(result["truncate"], true);
        let retained = &result["result"]["detail"]["accessibility_tree"];
        assert_eq!(retained["kind"], "aria_fragments");
        let fragments = retained["fragments"].as_array().unwrap();
        assert!(!fragments.is_empty());
        assert_eq!(fragments.first().unwrap()["start_line"], 1);
        assert_eq!(fragments.last().unwrap()["end_line"], 101);
        assert!(
            result["truncate_info"]["ranges"]["accessibility_tree"]
                .get("removed_line_start")
                .is_some()
        );
        for fragment in fragments {
            if let Some(text) = fragment["text"].as_str() {
                assert!(text.lines().skip(1).all(|line| line.starts_with("  - ")));
            }
        }
    }

    #[test]
    fn all_file_list_like_tools_remove_the_oldest_complete_item() {
        for (tool, field) in [
            ("File.List", "entries"),
            ("File.Find", "results"),
            ("File.Stat", "entries"),
        ] {
            let items = (0..300)
                .map(|index| {
                    if field == "results" {
                        Value::String(format!("{index:04}-{}", "x".repeat(100)))
                    } else {
                        json!({"path":format!("{index:04}"),"value":"x".repeat(100)})
                    }
                })
                .collect::<Vec<_>>();
            let result = truncate_for_model_with_limit(tool, wrapper(json!({field:items})), 700);
            assert_eq!(result["truncate"], true, "tool={tool}");
            let retained = result["result"]["detail"][field].as_array().unwrap();
            assert!(!retained.is_empty(), "tool={tool}");
            assert_ne!(
                retained.first().unwrap(),
                &json!(items_for_first(field)),
                "tool={tool}"
            );
        }
    }

    fn items_for_first(field: &str) -> Value {
        if field == "results" {
            Value::String(format!("0000-{}", "x".repeat(100)))
        } else {
            json!({"path":"0000","value":"x".repeat(100)})
        }
    }

    #[test]
    fn file_search_keeps_matches_as_whole_objects() {
        let matches = (0..300)
            .map(|index| {
                let line = index + 1;
                json!({
                    "path":"src/a.rs","column":1,"match_length":5,
                    "match_text":{line.to_string():format!("match-{index} {}", "x".repeat(100))},
                    "before":{},"after":{}
                })
            })
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "File.Search",
            wrapper(json!({"path":".","matches":matches,"skipped_binary":0,"truncated":false})),
            800,
        );
        assert_eq!(result["truncate"], true);
        let retained = result["result"]["detail"]["matches"].as_array().unwrap();
        assert!(
            retained.first().unwrap()["match_text"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
                > 1
        );
        assert!(retained.iter().all(|item| item.get("path").is_some()
            && item.get("hash").is_none()
            && item.get("match_text").is_some()));
    }

    #[test]
    fn single_huge_search_match_preserves_the_match_and_outer_context_semantics() {
        let text = format!("HEAD{}NEEDLE{}TAIL", "甲".repeat(8_000), "乙".repeat(8_000));
        let column = text.chars().position(|character| character == 'N').unwrap() + 1;
        let before = (1..=20)
            .map(|line| (line.to_string(), json!(format!("before-{line}"))))
            .collect::<Map<_, _>>();
        let after = (22..=41)
            .map(|line| (line.to_string(), json!(format!("after-{line}"))))
            .collect::<Map<_, _>>();
        let result = truncate_for_model_with_limit(
            "File.Search",
            wrapper(json!({"matches":[{
                "path":"a.txt","column":column,"match_length":6,
                "match_text":{"21":text},"before":before,"after":after
            }],"truncated":false})),
            850,
        );
        assert_eq!(result["truncate"], true);
        let item = &result["result"]["detail"]["matches"][0];
        let before = item["before"].as_object().unwrap();
        let after = item["after"].as_object().unwrap();
        assert!(before.is_empty() || before.contains_key("20"));
        assert!(after.is_empty() || after.contains_key("22"));
        let retained = item["match_text"]["21"]["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|fragment| fragment["text"].as_str())
            .collect::<String>();
        assert!(retained.starts_with("HEAD"));
        assert!(retained.contains("NEEDLE"));
        assert!(retained.ends_with("TAIL"));
    }

    #[test]
    fn a_pathologically_large_match_keeps_both_match_edges_without_stalling() {
        let matched = format!("MATCH-BEGIN{}MATCH-END", "M".repeat(30_000));
        let text = format!("line-head {matched} line-tail");
        let result = truncate_for_model_with_limit(
            "File.Search",
            wrapper(json!({"matches":[{
                "path":"a.txt","column":11,"match_length":matched.chars().count(),
                "match_text":{"1":text},"before":{},"after":{}
            }],"truncated":false})),
            1_000,
        );
        assert_eq!(result["truncate"], true);
        let fragments = result["result"]["detail"]["matches"][0]["match_text"]["1"]["fragments"]
            .as_array()
            .unwrap();
        let retained = fragments
            .iter()
            .filter_map(|fragment| fragment["text"].as_str())
            .collect::<String>();
        assert!(retained.contains("MATCH-BEGIN"));
        assert!(retained.contains("MATCH-END"));
        assert!(result["truncate_info"]["ranges"]["match_text"]["removed_byte_start"].is_number());
        assert!(result["truncate_info"]["ranges"]["match_text"]["removed_byte_end"].is_number());
    }

    #[test]
    fn terminal_create_and_list_use_their_own_safe_units() {
        let updates = (0..100)
            .map(|sequence| json!({"sequence":sequence,"styles":[],"rows":[{"terminal_row":sequence,"text":"x".repeat(200)}]}))
            .collect::<Vec<_>>();
        let created = truncate_for_model_with_limit(
            "Terminal.Create",
            json!({"terminal_updates":updates,"result":{"state":"succeeded","detail":{"session_id":"pty-1","changed_rows":1}}}),
            800,
        );
        assert_eq!(created["truncate"], true);
        assert_eq!(created["result"]["detail"]["session_id"], "pty-1");
        assert!(created["terminal_updates"][0]["sequence"].as_u64().unwrap() > 0);

        let sessions = (0..200)
            .map(|index| json!({"session_id":format!("pty-{index}"),"cwd":"x".repeat(100)}))
            .collect::<Vec<_>>();
        let listed = truncate_for_model_with_limit(
            "Terminal.List",
            wrapper(json!({"sessions":sessions})),
            700,
        );
        assert_eq!(listed["truncate"], true);
        assert_ne!(
            listed["result"]["detail"]["sessions"][0]["session_id"],
            "pty-0"
        );
    }

    #[test]
    fn web_page_lists_keep_active_and_human_changes_are_transactional() {
        let pages = (0..200)
            .map(|index| json!({"page_id":format!("p{index:07}"),"url":format!("https://example.test/{index}/{}", "x".repeat(100)),"title":"title","state":"open"}))
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "WebBrowser.Pages",
            wrapper(json!({"pages":pages,"active_page_id":"p0000000"})),
            900,
        );
        assert_eq!(result["truncate"], true);
        assert!(
            result["result"]["detail"]["pages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|page| page["page_id"] == "p0000000")
        );

        let changed = (1..100)
            .map(|index| json!({"page_id":format!("p{index:07}"),"change":"changed","page":{"page_id":format!("p{index:07}"),"url":"x".repeat(100)}}))
            .collect::<Vec<_>>();
        let opened = (1..100)
            .map(|index| json!({"page_id":format!("p{index:07}"),"page":{"page_id":format!("p{index:07}"),"url":"x".repeat(100)}}))
            .collect::<Vec<_>>();
        let handoff = truncate_for_model_with_limit(
            "WebBrowser.RequireHumanAction",
            wrapper(
                json!({"page_id":"p0000000","active_page_id":"p0000000","target_page":{"page_id":"p0000000"},"changed_pages":changed,"opened_pages":opened,"closed_page_ids":[]}),
            ),
            1000,
        );
        assert_eq!(handoff["truncate"], true);
        let changed_ids = handoff["result"]["detail"]["changed_pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["page_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let opened_ids = handoff["result"]["detail"]["opened_pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["page_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(changed_ids, opened_ids);
    }

    #[test]
    fn snapshot_drops_oldest_events_without_touching_the_screen_path() {
        let events = (0..300)
            .map(|index| json!({"kind":"console","message":format!("event-{index} {}", "x".repeat(100))}))
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "WebBrowser.Snapshot",
            wrapper(json!({
                "kind":"screen","screen_path":".me/webbrowser/screenshots/keep.png",
                "browser_events":events,"dismissed_native_dialogs":[]
            })),
            800,
        );
        assert_eq!(result["truncate"], true);
        assert_eq!(
            result["result"]["detail"]["screen_path"],
            ".me/webbrowser/screenshots/keep.png"
        );
        let retained = result["result"]["detail"]["browser_events"]
            .as_array()
            .unwrap();
        assert!(
            !retained[0]["message"]
                .as_str()
                .unwrap()
                .starts_with("event-0 ")
        );
        assert!(
            retained.last().unwrap()["message"]
                .as_str()
                .unwrap()
                .starts_with("event-299 ")
        );
    }

    #[test]
    fn click_drops_oldest_opened_page_ids() {
        let ids = (0..1000)
            .map(|index| Value::String(format!("p{index:07}")))
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "WebBrowser.Click",
            wrapper(json!({"page_id":"p9999999","clicked":true,"opened_page_ids":ids})),
            600,
        );
        assert_eq!(result["truncate"], true);
        assert_ne!(result["result"]["detail"]["opened_page_ids"][0], "p0000000");
    }

    #[test]
    fn agent_wait_crops_progress_and_preserves_final_answer() {
        let progress = (0..200)
            .map(|index| json!({"assistant_text":format!("step-{index} {}", "x".repeat(100)),"tool_calls":["File.Read"]}))
            .collect::<Vec<_>>();
        let result = truncate_for_model_with_limit(
            "Agent.Wait",
            wrapper(
                json!({"session_id":"agent-1","state":"completed","turn_id":1,"final_answer":"done","progress":progress,"context_usage":null,"compact_count":0}),
            ),
            700,
        );
        assert_eq!(result["truncate"], true);
        assert_eq!(result["result"]["detail"]["session_id"], "agent-1");
        assert_eq!(result["result"]["detail"]["final_answer"], "done");
    }

    #[test]
    fn agent_wait_removes_only_complete_progress_steps_and_crops_final_answer_in_the_middle() {
        let final_answer = format!("ANSWER-START\n{}\nANSWER-END", "正文".repeat(20_000));
        let result = truncate_for_model_with_limit(
            "Agent.Wait",
            wrapper(json!({
                "session_id":"agent-1","state":"completed","turn_id":9,
                "progress":[{"assistant_text":"one giant step","tool_calls":["Terminal.Interact","File.Read"]}],
                "final_answer":final_answer,"context_usage":{"input_tokens":12},"compact_count":2
            })),
            700,
        );
        assert_eq!(result["truncate"], true);
        assert!(
            result["result"]["detail"]["progress"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let answer = &result["result"]["detail"]["final_answer"];
        assert_eq!(answer["kind"], "text_fragments");
        let fragments = answer["fragments"].as_array().unwrap();
        assert!(
            fragments.first().unwrap()["text"]
                .as_str()
                .unwrap()
                .starts_with("ANSWER-START")
        );
        assert!(
            fragments.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .ends_with("ANSWER-END")
        );
        assert_eq!(result["result"]["detail"]["compact_count"], 2);
    }

    #[test]
    fn worker_wait_is_never_safety_truncated() {
        let progress = (0..300)
            .map(|index| {
                json!({
                    "assistant_text": format!("step-{index} {}", "完整内容".repeat(200)),
                    "tool_calls": ["Terminal.Interact", "File.Read"]
                })
            })
            .collect::<Vec<_>>();
        let original = wrapper(json!({
            "worker":"worker",
            "state":"completed",
            "turn_id":9,
            "progress":progress,
            "final_answer":format!("BEGIN\n{}\nEND", "完整回答".repeat(20_000)),
            "context_usage":{"input_tokens":12,"output_tokens":34,"total_tokens":46},
            "compact_count":2
        }));

        let result = truncate_for_model_with_limit("Worker.Wait", original.clone(), 100);

        assert_eq!(result["truncate"], false);
        assert!(result.get("truncate_info").is_none());
        assert_eq!(result["result"], original["result"]);
        assert!(estimate_tokens(&result) > 100);
    }

    #[test]
    fn workmap_read_drops_notes_without_dangling_plan_references() {
        let notes = (0..300)
            .map(|index| json!({"id":format!("note-{index:08x}"),"plan_id":"plan-00000001","created_at_ms":index,"content":"x".repeat(100)}))
            .collect::<Vec<_>>();
        let current = json!({
            "objective":{"id":"objective-00000001","state":"active","title":"test"},
            "plans":[{"plan":{"id":"plan-00000001","objective_id":"objective-00000001","state":"active","title":"plan"},"notes":notes}]
        });
        let result = truncate_for_model_with_limit(
            "WorkMap.Read",
            wrapper(json!({"memory":{"facts":[],"agreements":[]},"current":current})),
            900,
        );
        assert_eq!(result["truncate"], true);
        let detail = result["result"]["detail"].as_object().unwrap();
        assert!(workmap_references_are_closed(detail));
        let retained = detail["current"]["plans"][0]["notes"].as_array().unwrap();
        assert!(retained.first().unwrap()["created_at_ms"].as_u64().unwrap() > 0);
    }

    #[test]
    fn workmap_history_and_mutation_preserve_transaction_records() {
        let objectives = (0..200)
            .map(|index| json!({"objective":{"id":format!("objective-{index:08x}"),"title":"x".repeat(100)},"plan_count":1,"note_count":1}))
            .collect::<Vec<_>>();
        let history = truncate_for_model_with_limit(
            "WorkMap.ReadHistory",
            wrapper(json!({"objectives":objectives})),
            700,
        );
        assert_eq!(history["truncate"], true);
        assert_ne!(
            history["result"]["detail"]["objectives"][0]["objective"]["id"],
            "objective-00000000"
        );

        let records = vec![
            json!({"kind":"note","id":"note-00000001","plan_id":"plan-00000001","content":"committed"}),
        ];
        let notes = (0..200)
            .map(|index| json!({"id":format!("note-{index:08x}"),"plan_id":"plan-00000001","created_at_ms":index,"content":"x".repeat(100)}))
            .collect::<Vec<_>>();
        let mutation = truncate_for_model_with_limit(
            "WorkMap.AddNote",
            wrapper(json!({
                "memory":{"facts":[],"agreements":[]},
                "current":{"objective":{"id":"objective-00000001"},"plans":[{"plan":{"id":"plan-00000001","objective_id":"objective-00000001","state":"active"},"notes":notes}]},
                "records":records
            })),
            900,
        );
        assert_eq!(mutation["truncate"], true);
        assert_eq!(
            mutation["result"]["detail"]["records"][0]["content"],
            "committed"
        );
    }

    #[test]
    fn workmap_keeps_record_references_and_the_ends_of_memory() {
        let plans = (0..25)
            .map(|index| json!({
                "plan":{"id":format!("plan-{index:08x}"),"objective_id":"objective-00000001","state":if index == 0 {"active"} else {"planned"},"description":"x".repeat(200)},
                "notes":[]
            }))
            .collect::<Vec<_>>();
        let facts = (0..100)
            .map(|index| json!({"id":format!("memory-{index:08x}"),"content":format!("fact-{index} {}", "x".repeat(100))}))
            .collect::<Vec<_>>();
        let protected = "plan-0000000c";
        let result = truncate_for_model_with_limit(
            "WorkMap.AddNote",
            wrapper(json!({
                "memory":{"facts":facts,"agreements":[]},
                "current":{"objective":{"id":"objective-00000001"},"plans":plans},
                "records":[{"kind":"note","id":"note-00000001","plan_id":protected,"content":"transaction evidence"}]
            })),
            1_500,
        );
        assert_eq!(result["truncate"], true);
        let detail = result["result"]["detail"].as_object().unwrap();
        assert!(workmap_references_are_closed(detail));
        assert!(
            detail["current"]["plans"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["plan"]["id"] == protected)
        );
        assert_eq!(detail["records"][0]["content"], "transaction evidence");
        let retained = detail["memory"]["facts"].as_array().unwrap();
        assert_eq!(retained.first().unwrap()["id"], "memory-00000000");
        assert_eq!(retained.last().unwrap()["id"], "memory-00000063");
    }

    #[test]
    fn workmap_repeatedly_crops_one_huge_text_field_without_touching_records() {
        let detail = json!({
            "memory":{"facts":[],"agreements":[]},
            "current":{
                "objective":{"id":"objective-00000001","description":format!("BEGIN{}END", "长".repeat(30_000))},
                "plans":[{"plan":{"id":"plan-00000001","objective_id":"objective-00000001","state":"active"},"notes":[]}]
            },
            "records":[{"kind":"objective","id":"objective-00000001","description":"exact transaction"}]
        });
        let result = truncate_for_model_with_limit("WorkMap.Start", wrapper(detail), 1_000);
        assert_eq!(result["truncate"], true);
        let description = &result["result"]["detail"]["current"]["objective"]["description"];
        assert_eq!(description["kind"], "text_fragments");
        let fragments = description["fragments"].as_array().unwrap();
        assert!(
            fragments.first().unwrap()["text"]
                .as_str()
                .unwrap()
                .starts_with("BEGIN")
        );
        assert!(
            fragments.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .ends_with("END")
        );
        assert_eq!(
            result["result"]["detail"]["records"][0]["description"],
            "exact transaction"
        );
    }

    #[test]
    fn workmap_keeps_a_huge_mutation_record_but_safely_fragments_its_text() {
        let result = truncate_for_model_with_limit(
            "WorkMap.AddNote",
            wrapper(json!({
                "memory":{"facts":[],"agreements":[]},"current":null,
                "records":[{
                    "kind":"note","id":"note-00000001","plan_id":"plan-00000001",
                    "content":format!("RECORD-BEGIN{}RECORD-END", "证据".repeat(30_000))
                }]
            })),
            1_000,
        );
        assert_eq!(result["truncate"], true);
        let record = &result["result"]["detail"]["records"][0];
        assert_eq!(record["id"], "note-00000001");
        assert_eq!(record["plan_id"], "plan-00000001");
        assert_eq!(record["content"]["kind"], "text_fragments");
        let fragments = record["content"]["fragments"].as_array().unwrap();
        assert!(
            fragments.first().unwrap()["text"]
                .as_str()
                .unwrap()
                .starts_with("RECORD-BEGIN")
        );
        assert!(
            fragments.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .ends_with("RECORD-END")
        );
    }

    #[test]
    fn every_workmap_tool_routes_through_the_structure_aware_strategy() {
        for tool in [
            "WorkMap.Read",
            "WorkMap.ReadHistory",
            "WorkMap.Start",
            "WorkMap.UpdatePlanState",
            "WorkMap.AddNote",
            "WorkMap.ChangePlan",
            "WorkMap.AddPlan",
            "WorkMap.CloseObjective",
            "WorkMap.AddMemory",
            "WorkMap.InvalidateMemory",
        ] {
            let result = truncate_for_model_with_limit(
                tool,
                wrapper(json!({
                    "current":{
                        "objective":{"id":"objective-00000001","description":format!("BEGIN{}END", "x".repeat(20_000))},
                        "plans":[{"plan":{"id":"plan-00000001","objective_id":"objective-00000001","state":"active"},"notes":[]}]
                    }
                })),
                800,
            );
            assert_eq!(result["truncate"], true, "tool={tool}");
            assert_eq!(
                result["truncate_info"]["strategy"], "structure_aware",
                "tool={tool}"
            );
        }
    }

    #[test]
    fn default_limit_includes_final_truncation_metadata() {
        let lines = (0..50_000)
            .map(|index| format!("line {index} {}\n", "数据".repeat(8)))
            .collect::<Vec<_>>();
        let result = truncate_for_model(
            "File.Read",
            wrapper(
                json!({"lines":numbered_lines(lines, 1),"start_line":1,"end_line":50_000,"total_lines":50_000}),
            ),
            crate::toolbox::DEFAULT_TOOL_RESULT_TOKEN_LIMIT,
        );
        assert_eq!(result["truncate"], true);
        assert!(estimate_tokens(&result) <= crate::toolbox::DEFAULT_TOOL_RESULT_TOKEN_LIMIT);
    }

    #[test]
    fn registered_tool_with_no_safe_unit_returns_original_after_stall() {
        let original = wrapper(json!({"protected":"x".repeat(20_000),"entries":[]}));
        let result = truncate_for_model_with_limit("File.List", original.clone(), 1000);
        assert_eq!(result["truncate"], false);
        assert_eq!(result["result"], original["result"]);
    }

    #[test]
    fn token_progress_accepts_increases_and_only_stalls_on_equal_counts() {
        let mut stalled = 99;
        assert!(!update_stalled_attempts(10, 11, &mut stalled));
        assert_eq!(stalled, 0);
        for attempt in 1..MAX_STALLED_ATTEMPTS {
            assert!(!update_stalled_attempts(11, 11, &mut stalled));
            assert_eq!(stalled, attempt);
        }
        assert!(update_stalled_attempts(11, 11, &mut stalled));
    }
}
