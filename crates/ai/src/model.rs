//! Provider-neutral chat types the agent loop speaks; each backend maps them to its wire format.

use craft_anything_evidence::ToolError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One piece of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    /// Model reasoning. Kept verbatim (with its signature) and replayed on tool turns.
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// What the server attached to the tool call `tool_use_id`, sent back with it unchanged
    /// (Gemini's thought signature: a call without it is refused).
    ToolExtra {
        tool_use_id: String,
        extra: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// A picture shown to the model, in a person's message. Its bytes live only in memory: a
    /// saved transcript keeps `name`, and a resumed conversation sends [`Block::image_note`] in
    /// its place, so a dozen screenshots don't grow the file that's rewritten after every turn.
    /// Make one with [`Block::image`] or [`Block::image_file`].
    Image {
        /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`, read from the bytes.
        media_type: String,
        /// What the picture is, for the note that replaces it (a file name or a few words).
        name: String,
        /// The bytes, base64-encoded; empty once saved and loaded again.
        #[serde(skip)]
        data: String,
    },
}

/// The largest picture sent to a model, in bytes: base64 makes it 5 MB, the most Anthropic
/// takes for one picture (OpenAI-style servers take more).
pub const MAX_IMAGE_BYTES: usize = 3_750_000;

/// About how many tokens a picture takes in a model's context (a screenshot of about 1,100 ×
/// 1,100 at Anthropic's 750 pixels a token; local vision models are in the same range).
pub const IMAGE_TOKENS: u64 = 1_600;

/// The kind of picture `bytes` hold, read from their first bytes, or `None` for anything else.
pub fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}

impl Block {
    /// A picture from its bytes, refused when it isn't a PNG, JPEG, GIF or WebP, or is larger
    /// than [`MAX_IMAGE_BYTES`].
    pub fn image(name: impl Into<String>, bytes: &[u8]) -> Result<Block, ToolError> {
        use base64::Engine as _;
        let name = name.into();
        let Some(media_type) = image_media_type(bytes) else {
            return Err(ToolError::invalid(format!("{name} isn't a PNG, JPEG, GIF or WebP picture, so it can't be shown to the model."))
                .with_remediation("Save it as a PNG or JPEG and try again."));
        };
        if bytes.len() > MAX_IMAGE_BYTES {
            // Only the limit: `image_file` reads one byte past it, so the size read isn't the file's.
            return Err(ToolError::invalid(format!("{name} is more than the {} MB a picture shown to a model may be.", MAX_IMAGE_BYTES as f64 / 1_000_000.0))
                .with_remediation("Use a smaller picture: a lower resolution, or a JPEG."));
        }
        Ok(Block::Image { media_type: media_type.into(), name, data: base64::engine::general_purpose::STANDARD.encode(bytes) })
    }

    /// A picture read from `path`, named by its file name. Reads at most one byte more than
    /// [`MAX_IMAGE_BYTES`], so a huge file is refused without being loaded.
    pub fn image_file(path: &std::path::Path) -> Result<Block, ToolError> {
        use std::io::Read as _;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "the picture".into());
        let file = std::fs::File::open(path).map_err(|e| ToolError::io(format!("couldn't open {name}"), e))?;
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| ToolError::io(format!("couldn't read {name}"), e))?;
        Block::image(name, &bytes)
    }

    /// What a model is sent in place of a picture whose bytes aren't here (a resumed or trimmed
    /// conversation).
    pub fn image_note(name: &str) -> String {
        format!("[A picture was shown here ({name}); it isn't kept once the conversation is saved or shortened.]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    pub role: Speaker,
    pub blocks: Vec<Block>,
}

impl Msg {
    pub fn user_text(t: impl Into<String>) -> Self {
        Msg { role: Speaker::User, blocks: vec![Block::Text { text: t.into() }] }
    }

    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_uses(&self) -> Vec<(String, String, Value)> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect()
    }
}

/// A tool as offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Shared thinking levels for API backends and agent CLI variants, ordered least to most.
/// Claude maps unsupported levels down with [`Effort::within`]; OpenCode requires the exact
/// variant in its live catalogue. The `effort` and `cli_thinking` settings remain independent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    None,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    /// Longer-horizon work than `high`.
    Xhigh,
    /// The deepest reasoning, with no limit on tokens spent.
    Max,
}

impl Effort {
    /// The API/Claude levels, least to most.
    pub const ALL: [Effort; 5] = [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];

    pub const CLI_ALL: [Effort; 7] = [Effort::None, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];

    /// The stable lowercase id used in settings, API requests and agent model references.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// Parse a known level, ignoring surrounding whitespace and case.
    pub fn parse(s: &str) -> Option<Effort> {
        let s = s.trim();
        Effort::CLI_ALL.into_iter().find(|e| e.as_str().eq_ignore_ascii_case(s))
    }

    /// The display label for this level in model pickers and validation messages.
    pub fn title(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Xhigh => "Xhigh",
            Self::Max => "Max",
        }
    }

    /// This level on a model that offers only `levels`: the nearest one at or below it (as Claude
    /// Code does), else the lowest it has. Unchanged when `levels` is empty.
    pub fn within(self, levels: &[Effort]) -> Effort {
        levels.iter().copied().filter(|l| *l <= self).max().or_else(|| levels.iter().copied().min()).unwrap_or(self)
    }
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    PauseTurn,
    StopSequence,
    Other(String),
}

impl StopReason {
    pub fn parse(s: &str) -> StopReason {
        match s {
            "end_turn" | "stop" => StopReason::EndTurn,
            "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
            "max_tokens" | "length" => StopReason::MaxTokens,
            "refusal" | "content_filter" => StopReason::Refusal,
            "pause_turn" => StopReason::PauseTurn,
            "stop_sequence" => StopReason::StopSequence,
            other => StopReason::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::PauseTurn => "pause_turn",
            StopReason::StopSequence => "stop_sequence",
            StopReason::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, o: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(o.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(o.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(o.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(o.cache_write_tokens);
    }
}

/// One request to a chat backend.
#[derive(Debug, Clone)]
pub struct TurnRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [Msg],
    pub tools: &'a [ToolDef],
    pub effort: Effort,
    pub max_tokens: u32,
}

/// One response.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    pub message: Msg,
    pub stop: StopReason,
    pub usage: Usage,
    pub model: String,
    /// A provider-specific instruction to send to the model before continuing.
    pub follow_up: Option<String>,
}

/// Streaming pieces while a turn is generated.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Thinking(String),
    /// A line for the feed about the request itself, not the model's words (a wait for its rate limit).
    Status(String),
}

/// Cooperative cancellation for a conversation.
#[derive(Debug, Clone, Default)]
pub struct Stop {
    stopped: Arc<AtomicBool>,
    tool: Option<craft_anything_engine::CancelToken>,
}

impl Stop {
    /// Link discovery to cancellation of the engine tool that owns it.
    pub fn for_tool(cancel: craft_anything_engine::CancelToken) -> Self {
        Self { tool: Some(cancel), ..Default::default() }
    }

    /// Request cancellation for every clone sharing this conversation's stop flag.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Clear a previous stop (at the start of the next message).
    pub fn reset(&self) {
        self.stopped.store(false, Ordering::SeqCst);
    }

    /// Whether the conversation or its owning engine tool has requested cancellation.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.tool.as_ref().is_some_and(|c| c.is_cancelled())
    }

    pub fn check(&self) -> Result<(), ToolError> {
        if self.is_stopped() { Err(ToolError::cancelled()) } else { Ok(()) }
    }
}

/// A model that can take one turn. Implemented by the HTTP backends (and fakes in tests).
pub trait ChatModel: Send {
    fn turn(&mut self, req: &TurnRequest<'_>, on_delta: &mut dyn FnMut(Delta), stop: &Stop) -> Result<TurnResult, ToolError>;
    /// Model id, for display, journal and pricing.
    fn model(&self) -> String;
    /// The context window its server says it has loaded, when it says one (see [`crate::window`]).
    /// Asked on the agent's worker, never on the UI thread.
    fn context_window(&self) -> Option<crate::window::Window> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn a_pictures_kind_is_read_from_its_bytes() {
        assert_eq!(image_media_type(PNG), Some("image/png"));
        assert_eq!(image_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_media_type(b"GIF89a\x01\0"), Some("image/gif"));
        assert_eq!(image_media_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        for not in [b"".as_slice(), b"\x89PN", b"RIFF\0\0\0\0WAVE", b"RIFF\0\0\0\0WEB", b"<svg xmlns=", b"BM6\0\0\0"] {
            assert_eq!(image_media_type(not), None, "{not:?}");
        }
    }

    #[test]
    fn a_picture_is_refused_when_it_isnt_one_or_is_too_big() {
        let Block::Image { media_type, name, data } = Block::image("now.png", PNG).unwrap() else { panic!() };
        assert_eq!((media_type.as_str(), name.as_str()), ("image/png", "now.png"));
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(&data).unwrap(), PNG);
        // The kind comes from the bytes, never the name.
        let e = Block::image("fake.png", b"not a picture").unwrap_err();
        assert!(e.message.contains("isn't a PNG, JPEG, GIF or WebP") && e.remediation.is_some(), "{e:?}");
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES, 0);
        assert!(Block::image("big.png", &big).is_ok(), "exactly the limit is taken");
        big.push(0);
        let e = Block::image("big.png", &big).unwrap_err();
        assert!(e.message.contains("more than the 3.75 MB"), "{e:?}");
        // Base64 of the largest picture fits Anthropic's 5 MB.
        assert!(MAX_IMAGE_BYTES.div_ceil(3) * 4 <= 5 * 1024 * 1024);
    }

    #[test]
    fn a_picture_file_is_read_up_to_the_limit_and_a_missing_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("now.png");
        std::fs::write(&small, PNG).unwrap();
        assert!(matches!(Block::image_file(&small).unwrap(), Block::Image { name, .. } if name == "now.png"));
        let huge = dir.path().join("huge.png");
        let mut bytes = PNG.to_vec();
        bytes.resize(MAX_IMAGE_BYTES * 2, 0);
        std::fs::write(&huge, &bytes).unwrap();
        let e = Block::image_file(&huge).unwrap_err();
        assert_eq!(e.message, "huge.png is more than the 3.75 MB a picture shown to a model may be.");
        assert!(Block::image_file(&dir.path().join("gone.png")).is_err());
        assert!(Block::image_file(dir.path()).is_err(), "a folder isn't a picture");
    }

    #[test]
    fn a_saved_picture_keeps_its_name_but_not_its_bytes() {
        let msg = Msg { role: Speaker::User, blocks: vec![Block::Text { text: "Here:".into() }, Block::image("now.png", PNG).unwrap()] };
        let saved = serde_json::to_string(&msg).unwrap();
        assert!(saved.contains("\"name\":\"now.png\"") && !saved.contains("\"data\""), "{saved}");
        let back: Msg = serde_json::from_str(&saved).unwrap();
        assert_eq!(back.blocks[1], Block::Image { media_type: "image/png".into(), name: "now.png".into(), data: String::new() });
        assert_eq!(back.text(), "Here:", "a picture isn't text");
    }
}
