//! Session history: page fetch and text rendering of ACP JSON-RPC items.

use super::CliError;
use crate::{
    api::spawner::{MAX_HISTORY_PAGE, SessionHistoryMessage, SpawnerApi},
    oneshot::{ChunkType, ConversationPrinter},
};
use serde_json::Value as JsonValue;
use std::{collections::HashMap, io::Write};
use terminal_size::terminal_size;

/// Page size for incremental fetches
const FOLLOW_PAGE: u64 = 100;

/// Fetches history items in chronological order.
///
/// The server sorts pages from the newest item to the oldest. `tail` limits the result to the last N items.
pub fn fetch(
    api: &SpawnerApi,
    id: &str,
    tail: Option<u64>,
) -> Result<Vec<SessionHistoryMessage>, CliError> {
    let mut items = Vec::new();
    let mut offset = 0;
    loop {
        let limit = match tail {
            Some(tail) => tail.saturating_sub(offset).min(MAX_HISTORY_PAGE),
            None => MAX_HISTORY_PAGE,
        };
        if limit == 0 {
            break;
        }
        let page = api.history(id, offset, limit)?.value;
        let count = page.messages.len() as u64;
        items.extend(page.messages);
        offset += count;
        if count == 0 || count < limit || offset >= page.pagination.total_items {
            break;
        }
    }
    items.sort_by_key(|m| m.index);
    Ok(items)
}

/// Fetches the items that are newer than `last_index`, in chronological order.
pub fn fetch_newer(
    api: &SpawnerApi,
    id: &str,
    last_index: Option<u64>,
) -> Result<Vec<SessionHistoryMessage>, CliError> {
    let Some(last_index) = last_index else {
        return fetch(api, id, None);
    };
    let mut items = Vec::new();
    let mut offset = 0;
    loop {
        let page = api.history(id, offset, FOLLOW_PAGE)?.value;
        let count = page.messages.len() as u64;
        let mut reached_known = false;
        for message in page.messages {
            if message.index > last_index {
                items.push(message);
            } else {
                reached_known = true;
            }
        }
        offset += count;
        if reached_known
            || count == 0
            || count < FOLLOW_PAGE
            || offset >= page.pagination.total_items
        {
            break;
        }
    }
    items.sort_by_key(|m| m.index);
    Ok(items)
}

/// Renders history items as a conversation.
pub struct HistoryRenderer<W: Write> {
    printer: ConversationPrinter<W>,
    /// Tool call titles by `toolCallId`. Updates often have no title.
    tool_titles: HashMap<String, String>,
}

impl<W: Write> HistoryRenderer<W> {
    pub fn new(writer: W) -> Self {
        let width = terminal_size().map(|(w, _)| w.0).unwrap_or(120) as usize;
        Self::with_width(width, writer)
    }

    pub fn with_width(width: usize, writer: W) -> Self {
        Self {
            printer: ConversationPrinter::new(width, writer),
            tool_titles: HashMap::new(),
        }
    }

    /// Renders one item. `message` is the JSON text of a JSON-RPC message.
    pub fn render(&mut self, message: &str) {
        let Ok(json) = serde_json::from_str::<JsonValue>(message) else {
            self.printer
                .print_block(ChunkType::Event, "item that is not JSON");
            return;
        };
        self.render_json(&json);
    }

    /// Ends the current line, for example before a status line.
    pub fn finish(&mut self) {
        self.printer.finish();
    }

    /// Writes a line without a header, after the current conversation line.
    pub fn plain_line(&mut self, text: &str) -> std::io::Result<()> {
        self.printer.finish();
        writeln!(self.printer.writer_mut(), "{text}")?;
        self.printer.writer_mut().flush()
    }

    fn render_json(&mut self, json: &JsonValue) {
        if let Some(method) = json.get("method").and_then(JsonValue::as_str) {
            let params = json.get("params").unwrap_or(&JsonValue::Null);
            match method {
                "session/update" => self.render_update(&params["update"]),
                "session/prompt" => {
                    let text = content_text(&params["prompt"]);
                    self.printer.print_block(ChunkType::User, &text);
                }
                other => self.printer.print_block(ChunkType::Event, other),
            }
        } else if let Some(result) = json.get("result") {
            match result.get("stopReason").and_then(JsonValue::as_str) {
                Some(reason) => self
                    .printer
                    .print_block(ChunkType::Event, &format!("stop reason: {reason}")),
                None => self.printer.print_block(ChunkType::Event, "response"),
            }
        } else if let Some(error) = json.get("error") {
            let message = error
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("no message");
            self.printer
                .print_block(ChunkType::Event, &format!("error: {message}"));
        } else {
            self.printer.print_block(ChunkType::Event, "unknown item");
        }
    }

    fn render_update(&mut self, update: &JsonValue) {
        let kind = update
            .get("sessionUpdate")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown update");
        match kind {
            "user_message_chunk" => self.render_chunk(ChunkType::User, update),
            "agent_message_chunk" => self.render_chunk(ChunkType::Agent, update),
            "agent_thought_chunk" => self.render_chunk(ChunkType::Thought, update),
            "tool_call" => {
                if let (Some(id), Some(title)) =
                    (str_field(update, "toolCallId"), str_field(update, "title"))
                {
                    self.tool_titles.insert(id.to_string(), title.to_string());
                }
                let title = str_field(update, "title")
                    .or_else(|| str_field(update, "toolCallId"))
                    .unwrap_or("tool call");
                let text = match str_field(update, "status") {
                    Some(status) => format!("{title} [{status}]"),
                    None => title.to_string(),
                };
                self.printer.print_block(ChunkType::Tool, &text);
            }
            "tool_call_update" => {
                if let (Some(id), Some(title)) =
                    (str_field(update, "toolCallId"), str_field(update, "title"))
                {
                    self.tool_titles.insert(id.to_string(), title.to_string());
                }
                let known = str_field(update, "toolCallId")
                    .and_then(|id| self.tool_titles.get(id))
                    .cloned();
                let title = str_field(update, "title")
                    .map(str::to_string)
                    .or(known)
                    .or_else(|| str_field(update, "toolCallId").map(str::to_string))
                    .unwrap_or("tool call".to_string());
                let text = match str_field(update, "status") {
                    Some(status) => format!("{title} -> {status}"),
                    None => format!("{title} (update)"),
                };
                self.printer.print_block(ChunkType::Tool, &text);
            }
            "plan" => {
                let entries = update["entries"].as_array().cloned().unwrap_or_default();
                let lines: Vec<String> = entries
                    .iter()
                    .map(|e| {
                        format!(
                            "[{}] {}",
                            str_field(e, "status").unwrap_or("?"),
                            str_field(e, "content").unwrap_or("")
                        )
                    })
                    .collect();
                let text = if lines.is_empty() {
                    "(empty plan)".to_string()
                } else {
                    lines.join("\n")
                };
                self.printer.print_block(ChunkType::Plan, &text);
            }
            other => self.printer.print_block(ChunkType::Event, other),
        }
    }

    fn render_chunk(&mut self, ty: ChunkType, update: &JsonValue) {
        let content = &update["content"];
        match content.get("type").and_then(JsonValue::as_str) {
            Some("text") => {
                let text = str_field(content, "text").unwrap_or("");
                self.printer.print(ty, text);
            }
            Some(other) => self.printer.print(ty, &format!("[{other}]")),
            None => self.printer.print(ty, "[no content]"),
        }
    }
}

fn str_field<'a>(value: &'a JsonValue, name: &str) -> Option<&'a str> {
    value.get(name).and_then(JsonValue::as_str)
}

/// Joins the text of ACP content blocks. Other blocks show as `[type]`.
fn content_text(blocks: &JsonValue) -> String {
    blocks
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| match str_field(b, "type") {
                    Some("text") => str_field(b, "text").unwrap_or("").to_string(),
                    Some(other) => format!("[{other}]"),
                    None => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(update: JsonValue) -> String {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": update}})
            .to_string()
    }

    fn render(items: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        {
            let mut renderer = HistoryRenderer::with_width(120, &mut out);
            for item in items {
                renderer.render(item);
            }
            renderer.finish();
        }
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn renders_message_chunks() {
        let out = render(&[
            update(
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Hi"}}),
            ),
            update(
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
            ),
            update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hel"}}),
            ),
            update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "lo"}}),
            ),
        ]);
        assert_eq!(out, vec!["user ▎Hi", "thought ▎hmm", "agent ▎Hello"]);
    }

    #[test]
    fn renders_non_text_content() {
        let out = render(&[update(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "x"}}),
        )]);
        assert_eq!(out, vec!["agent ▎[image]"]);
    }

    #[test]
    fn renders_tool_calls_each_on_own_line() {
        let out = render(&[
            update(
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read file", "status": "pending"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "title": "Read a.txt"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed"}),
            ),
        ]);
        assert_eq!(
            out,
            vec![
                "tool ▎Read file [pending]",
                "tool ▎Read file -> completed",
                "tool ▎Read a.txt (update)",
                "tool ▎Read a.txt -> failed",
                "tool ▎t2 -> completed"
            ]
        );
    }

    #[test]
    fn renders_plan() {
        let out = render(&[update(json!({"sessionUpdate": "plan", "entries": [
            {"content": "Step 1", "priority": "high", "status": "completed"},
            {"content": "Step 2", "priority": "low", "status": "pending"}
        ]}))]);
        assert_eq!(out, vec!["plan ▎[completed] Step 1", "▎[pending] Step 2"]);
    }

    #[test]
    fn renders_prompt_result_and_error() {
        let out = render(&[
            json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}).to_string(),
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -1, "message": "boom"}})
                .to_string(),
        ]);
        assert_eq!(
            out,
            vec!["event ▎stop reason: end_turn", "event ▎error: boom"]
        );
    }

    #[test]
    fn renders_prompt_request_as_user_text() {
        let out = render(&[
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": "s", "prompt": [{"type": "text", "text": "Next"}]}})
            .to_string(),
        ]);
        assert_eq!(out, vec!["user ▎Next"]);
    }

    #[test]
    fn renders_other_kinds_as_one_line() {
        let out = render(&[
            update(json!({"sessionUpdate": "available_commands_update", "availableCommands": []})),
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/request_permission", "params": {}}).to_string(),
            "not json".to_string(),
        ]);
        assert_eq!(
            out,
            vec![
                "event ▎available_commands_update",
                "event ▎session/request_permission",
                "event ▎item that is not JSON"
            ]
        );
    }
}
