//! Reading Freebuff's chat transcript.
//!
//! Freebuff writes `chat-messages.json` next to its `log.jsonl`: a JSON array
//! of chat messages. The shape is internal to the CLI and can change between
//! releases, so every field here is optional except what is used to address a
//! block, and an unparsable read is skipped rather than fatal.
//!
//! Observed shape on Freebuff 0.2.35:
//!
//! ```json
//! [
//!   {"id":"divider-1","variant":"ai","content":"","blocks":[{"type":"mode-divider","mode":"LITE"}]},
//!   {"id":"user-1","variant":"user","content":"the prompt"},
//!   {"id":"ai-1","variant":"ai","content":"","blocks":[
//!      {"type":"text","content":"thinking...","textType":"reasoning"},
//!      {"type":"tool","toolCallId":"hx..","toolName":"run_terminal_command",
//!       "input":{"command":"ls"},"output":"file.txt\n"},
//!      {"type":"text","content":"Done.","textType":"text"}
//!   ],"isComplete":true}
//! ]
//! ```

use std::collections::HashMap;

use serde::Deserialize;

/// One message in the transcript. Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub blocks: Option<Vec<Block>>,
}

/// One block inside an AI message.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: Option<String>,
    /// `"reasoning"` for the model's thinking, `"text"` for its answer.
    #[serde(default)]
    pub text_type: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    /// A tool result once it lands: a string on 0.2.35, never an object.
    #[serde(default)]
    pub output: Option<serde_json::Value>,
}

/// What the transcript says that the drive should report, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockEvent {
    Text(String),
    TextDelta(String),
    Reasoning(String),
    ReasoningDelta(String),
    ToolStarted {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        name: String,
        text: String,
    },
}

/// Longest text a single `tool_result` event carries. Freebuff tool output can
/// be megabytes; the console renders a card, not a file dump.
pub const TOOL_RESULT_LIMIT: usize = 120_000;

/// Parse the whole transcript file.
pub fn parse_messages(json: &str) -> Result<Vec<ChatMessage>, serde_json::Error> {
    serde_json::from_str::<Vec<ChatMessage>>(json)
}

#[derive(Debug, Default)]
struct ToolState {
    name: String,
    output: Option<String>,
}

/// Remembers what has already been reported, so repeated polls of the same
/// file only emit what is new.
#[derive(Debug, Default)]
pub struct TranscriptState {
    texts: HashMap<String, String>,
    tools: HashMap<String, ToolState>,
}

impl TranscriptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a freshly parsed transcript and get the events it has not produced
    /// before: a new block arrives whole, a block that grew arrives as a delta.
    pub fn ingest(&mut self, messages: &[ChatMessage]) -> Vec<BlockEvent> {
        let mut events = Vec::new();
        for message in messages {
            // The user's own prompt is already in the console's chat; only the
            // assistant side is news. A divider is a UI affordance, not content.
            if let Some(variant) = &message.variant {
                if variant != "ai" {
                    continue;
                }
            }

            let blocks = message.blocks.as_deref().unwrap_or(&[]);
            for (index, block) in blocks.iter().enumerate() {
                let key = format!("{}#{index}", message.id);
                match block.kind.as_str() {
                    "text" => {
                        let content = block.content.clone().unwrap_or_default();
                        let reasoning = block.text_type.as_deref() == Some("reasoning");
                        self.ingest_text(key, content, reasoning, &mut events);
                    }
                    "tool" => self.ingest_tool(key, block, &mut events),
                    // "mode-divider" and anything a later CLI adds.
                    _ => {}
                }
            }

            // A message with no blocks but a body: report the body as text.
            if blocks.is_empty() {
                if let Some(content) = &message.content {
                    if !content.is_empty() {
                        let key = format!("{}#content", message.id);
                        self.ingest_text(key, content.clone(), false, &mut events);
                    }
                }
            }
        }
        events
    }

    fn ingest_text(
        &mut self,
        key: String,
        content: String,
        reasoning: bool,
        events: &mut Vec<BlockEvent>,
    ) {
        if content.is_empty() {
            return;
        }
        match self.texts.get(&key) {
            None => {
                events.push(if reasoning {
                    BlockEvent::Reasoning(content.clone())
                } else {
                    BlockEvent::Text(content.clone())
                });
                self.texts.insert(key, content);
            }
            Some(seen) => {
                if content == *seen {
                    return;
                }
                if let Some(delta) = content.strip_prefix(seen.as_str()) {
                    events.push(if reasoning {
                        BlockEvent::ReasoningDelta(delta.to_string())
                    } else {
                        BlockEvent::TextDelta(delta.to_string())
                    });
                } else {
                    // The block was rewritten rather than appended to. Report
                    // the whole text again instead of inventing a diff.
                    events.push(if reasoning {
                        BlockEvent::Reasoning(content.clone())
                    } else {
                        BlockEvent::Text(content.clone())
                    });
                }
                self.texts.insert(key, content);
            }
        }
    }

    fn ingest_tool(&mut self, key: String, block: &Block, events: &mut Vec<BlockEvent>) {
        let tool_use_id = block
            .tool_call_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| key.clone());
        let name = block.tool_name.clone().unwrap_or_default();
        let state = self.tools.entry(tool_use_id.clone()).or_insert_with(|| {
            events.push(BlockEvent::ToolStarted {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                input: block.input.clone().unwrap_or(serde_json::Value::Null),
            });
            ToolState {
                name: name.clone(),
                output: None,
            }
        });

        if state.name.is_empty() && !name.is_empty() {
            state.name.clone_from(&name);
        }

        if let Some(output) = block.output.as_ref() {
            let text = value_to_text(output);
            if state.output.as_deref() != Some(text.as_str()) {
                events.push(BlockEvent::ToolResult {
                    tool_use_id,
                    name: if state.name.is_empty() {
                        name.clone()
                    } else {
                        state.name.clone()
                    },
                    text: clip_text(&text, TOOL_RESULT_LIMIT),
                });
                state.output = Some(text);
            }
        }
    }
}

/// A block's `output` as displayable text.
fn value_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn clip_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = text[..end].to_string();
    clipped.push_str("\n[... truncated by the Freebuff driver]");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"id":"divider-1","variant":"ai","content":"","blocks":[{"type":"mode-divider","mode":"LITE"}],"timestamp":"10:01 AM"},
      {"id":"user-1","variant":"user","content":"run git diff","timestamp":"10:01 AM"},
      {"id":"ai-1","variant":"ai","content":"","blocks":[
        {"type":"text","content":"Let me look.","textType":"reasoning","thinkingId":"thinking-1","thinkingOpen":false},
        {"type":"tool","toolCallId":"call-1","toolName":"run_terminal_command","input":{"command":"ls"},"agentId":"main-agent"},
        {"type":"text","content":"Done.","textType":"text"}
      ],"isComplete":true,"credits":0,"completionTime":"7s"}
    ]"#;

    #[test]
    fn a_new_transcript_reports_text_reasoning_and_tool_start() {
        let messages = parse_messages(SAMPLE).unwrap();
        let mut state = TranscriptState::new();
        let events = state.ingest(&messages);

        assert_eq!(
            events,
            vec![
                BlockEvent::Reasoning("Let me look.".into()),
                BlockEvent::ToolStarted {
                    tool_use_id: "call-1".into(),
                    name: "run_terminal_command".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
                BlockEvent::Text("Done.".into()),
            ]
        );
    }

    #[test]
    fn a_second_poll_of_the_same_file_is_silent() {
        let messages = parse_messages(SAMPLE).unwrap();
        let mut state = TranscriptState::new();
        assert!(!state.ingest(&messages).is_empty());
        assert!(state.ingest(&messages).is_empty());
    }

    #[test]
    fn a_grown_text_block_arrives_as_a_delta() {
        let mut state = TranscriptState::new();
        let first = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"text","content":"Hel","textType":"text"}]}]"#).unwrap();
        assert_eq!(state.ingest(&first), vec![BlockEvent::Text("Hel".into())]);

        let second = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"text","content":"Hello there","textType":"text"}]}]"#).unwrap();
        assert_eq!(
            state.ingest(&second),
            vec![BlockEvent::TextDelta("lo there".into())]
        );

        // Same length, same content: nothing new.
        assert!(state.ingest(&second).is_empty());
    }

    #[test]
    fn a_rewritten_block_reports_the_whole_text_again() {
        let mut state = TranscriptState::new();
        let first = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"text","content":"abc","textType":"text"}]}]"#).unwrap();
        state.ingest(&first);
        let second = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"text","content":"xyz","textType":"text"}]}]"#).unwrap();
        assert_eq!(state.ingest(&second), vec![BlockEvent::Text("xyz".into())]);
    }

    #[test]
    fn an_empty_text_block_is_not_reported_until_it_has_content() {
        let mut state = TranscriptState::new();
        let empty = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"text","content":"","textType":"text"}]}]"#).unwrap();
        assert!(state.ingest(&empty).is_empty());
        let grown = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"text","content":"now","textType":"text"}]}]"#).unwrap();
        assert_eq!(state.ingest(&grown), vec![BlockEvent::Text("now".into())]);
    }

    #[test]
    fn tool_output_is_reported_once_and_only_when_it_appears() {
        let mut state = TranscriptState::new();
        let started = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"tool","toolCallId":"c1","toolName":"read_files","input":{"path":"a.txt"}}]}]"#).unwrap();
        assert_eq!(
            state.ingest(&started),
            vec![BlockEvent::ToolStarted {
                tool_use_id: "c1".into(),
                name: "read_files".into(),
                input: serde_json::json!({"path": "a.txt"}),
            }]
        );

        let finished = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"tool","toolCallId":"c1","toolName":"read_files","input":{"path":"a.txt"},"output":"hello\n"}]}]"#).unwrap();
        assert_eq!(
            state.ingest(&finished),
            vec![BlockEvent::ToolResult {
                tool_use_id: "c1".into(),
                name: "read_files".into(),
                text: "hello\n".into(),
            }]
        );
        assert!(state.ingest(&finished).is_empty());
    }

    #[test]
    fn a_tool_result_that_grows_is_reported_in_full_again() {
        let mut state = TranscriptState::new();
        let partial = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"tool","toolCallId":"c1","toolName":"read_files","output":"one"}]}]"#).unwrap();
        state.ingest(&partial);
        let complete = parse_messages(r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"tool","toolCallId":"c1","toolName":"read_files","output":"one two"}]}]"#).unwrap();
        assert_eq!(
            state.ingest(&complete),
            vec![BlockEvent::ToolResult {
                tool_use_id: "c1".into(),
                name: "read_files".into(),
                text: "one two".into(),
            }]
        );
    }

    #[test]
    fn a_non_string_tool_output_is_reported_as_json() {
        let messages = parse_messages(
            r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"tool","toolCallId":"c1","toolName":"x","output":{"ok":true}}]}]"#,
        )
        .unwrap();
        let mut state = TranscriptState::new();
        let events = state.ingest(&messages);
        match events.last() {
            Some(BlockEvent::ToolResult { text, .. }) => assert_eq!(text, r#"{"ok":true}"#),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn a_user_message_is_never_reported_as_assistant_text() {
        let messages = parse_messages(
            r#"[{"id":"user-1","variant":"user","content":"do the thing","blocks":[{"type":"text","content":"do the thing","textType":"text"}]}]"#,
        )
        .unwrap();
        let mut state = TranscriptState::new();
        assert!(state.ingest(&messages).is_empty());
    }

    #[test]
    fn a_message_with_no_blocks_but_a_body_is_reported() {
        let messages =
            parse_messages(r#"[{"id":"ai-9","variant":"ai","content":"plain body"}]"#).unwrap();
        let mut state = TranscriptState::new();
        assert_eq!(
            state.ingest(&messages),
            vec![BlockEvent::Text("plain body".into())]
        );
    }

    #[test]
    fn unknown_block_types_and_fields_are_ignored() {
        let messages = parse_messages(
            r#"[{"id":"ai-1","variant":"ai","blocks":[{"type":"brand-new","content":"x","extra":1}],"somethingNew":true}]"#,
        )
        .unwrap();
        let mut state = TranscriptState::new();
        assert!(state.ingest(&messages).is_empty());
    }

    #[test]
    fn a_truncated_file_is_an_error_the_caller_can_skip() {
        assert!(parse_messages(r#"[{"id":"ai-1""#).is_err());
        assert!(parse_messages("").is_err());
        assert!(parse_messages("[]").unwrap().is_empty());
    }

    #[test]
    fn huge_tool_output_is_clipped_on_a_char_boundary() {
        let long = "é".repeat(TOOL_RESULT_LIMIT);
        let messages = format!(
            r#"[{{"id":"ai-1","variant":"ai","blocks":[{{"type":"tool","toolCallId":"c1","toolName":"x","output":"{long}"}}]}}]"#
        );
        let parsed = parse_messages(&messages).unwrap();
        let mut state = TranscriptState::new();
        let events = state.ingest(&parsed);
        match events.last() {
            Some(BlockEvent::ToolResult { text, .. }) => {
                assert!(text.len() < long.len() + 100);
                assert!(text.ends_with("[... truncated by the Freebuff driver]"));
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }
}
