//! Chat interface to a locally hosted model.

mod ollama;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use ollama::{parse_loaded_vram_mib, parse_model_profile, Ollama, DEFAULT_BASE_URL};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("model server request failed: {0}")]
    Http(#[from] ureq::Error),
    #[error("model '{0}' is not installed on the model server")]
    UnknownModel(String),
    #[error("unexpected model server response: {0}")]
    Response(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on `Role::Tool` messages: the tool whose result this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub fn tool_result(tool_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_name: Some(tool_name.into()),
            ..Self::new(Role::Tool, content)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// A tool offered to the model, described with a JSON Schema for its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    /// Layers to place on the GPU; `None` leaves the choice to the server.
    pub gpu_layers: Option<u32>,
    /// Context window to load the model with; `None` uses the server default.
    pub context_tokens: Option<u32>,
}

pub trait ChatBackend {
    fn chat(&self, request: &ChatRequest<'_>) -> Result<Message, LlmError>;
}
