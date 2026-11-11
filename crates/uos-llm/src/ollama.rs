use std::time::Duration;

use serde_json::{json, Value};
use uos_hw::ModelProfile;

use crate::{ChatBackend, ChatRequest, LlmError, Message, ToolSpec};

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

/// Loading a large model and reasoning over it can take minutes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
const BYTES_PER_MIB: u64 = 1024 * 1024;
const DEFAULT_TAG: &str = ":latest";
/// The agent acts on the model's output, so it should be repeatable.
const TEMPERATURE: f32 = 0.0;
/// The cache stores a key and a value per layer, each as 16-bit floats.
const CACHE_BYTES_PER_DIMENSION: u64 = 4;

pub struct Ollama {
    base_url: String,
    agent: ureq::Agent,
}

impl Ollama {
    pub fn new(base_url: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .into();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// Size and layer count of an installed model, for the tier planner.
    pub fn model_profile(&self, model: &str) -> Result<ModelProfile, LlmError> {
        let tags: Value = self
            .agent
            .get(format!("{}/api/tags", self.base_url))
            .call()?
            .body_mut()
            .read_json()?;
        let show = self.post("/api/show", &json!({ "model": model }));
        match show {
            Ok(show) => parse_model_profile(model, &tags, &show),
            Err(LlmError::Http(ureq::Error::StatusCode(404))) => {
                Err(LlmError::UnknownModel(model.to_string()))
            }
            Err(other) => Err(other),
        }
    }

    /// VRAM held by models the server has loaded. The server unloads them
    /// when another model needs the room.
    pub fn loaded_vram_mib(&self) -> Result<u64, LlmError> {
        let running: Value = self
            .agent
            .get(format!("{}/api/ps", self.base_url))
            .call()?
            .body_mut()
            .read_json()?;
        Ok(parse_loaded_vram_mib(&running))
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, LlmError> {
        Ok(self
            .agent
            .post(format!("{}{path}", self.base_url))
            .send_json(body)?
            .body_mut()
            .read_json()?)
    }
}

impl ChatBackend for Ollama {
    fn chat(&self, request: &ChatRequest<'_>) -> Result<Message, LlmError> {
        let reply = self.post("/api/chat", &chat_body(request))?;
        let message = reply
            .get("message")
            .cloned()
            .ok_or_else(|| LlmError::Response("reply has no message".into()))?;
        serde_json::from_value(message).map_err(|e| LlmError::Response(e.to_string()))
    }
}

fn chat_body(request: &ChatRequest<'_>) -> Value {
    let placement = [
        ("num_gpu", request.gpu_layers),
        ("num_ctx", request.context_tokens),
    ]
    .into_iter()
    .filter_map(|(key, value)| Some((key.to_string(), json!(value?))));
    let options: serde_json::Map<String, Value> =
        std::iter::once(("temperature".to_string(), json!(TEMPERATURE)))
            .chain(placement)
            .collect();
    json!({
        "model": request.model,
        "messages": request.messages,
        "tools": request.tools.iter().map(tool_json).collect::<Vec<_>>(),
        "options": options,
        "stream": false,
    })
}

fn tool_json(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

/// Builds a profile from the `/api/tags` and `/api/show` responses.
pub fn parse_model_profile(model: &str, tags: &Value, show: &Value) -> Result<ModelProfile, LlmError> {
    let with_tag = format!("{model}{DEFAULT_TAG}");
    let size_bytes = tags
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| {
            let name = entry.get("name").and_then(Value::as_str);
            name == Some(model) || name == Some(with_tag.as_str())
        })
        .and_then(|entry| entry.get("size").and_then(Value::as_u64))
        .ok_or_else(|| LlmError::UnknownModel(model.to_string()))?;

    let info = show.get("model_info").and_then(Value::as_object);
    let field = |suffix: &str| {
        info.into_iter()
            .flatten()
            .find(|(key, _)| key.ends_with(suffix))
            .and_then(|(_, value)| value.as_u64())
    };
    let required = |suffix: &str| {
        field(suffix)
            .ok_or_else(|| LlmError::Response(format!("no {suffix} reported for '{model}'")))
    };
    let blocks = required(".block_count")?;
    let embedding = required(".embedding_length")?;
    // With grouped-query attention only the key/value heads are cached.
    let cached_width = match (
        field(".attention.head_count"),
        field(".attention.head_count_kv"),
    ) {
        (Some(heads), Some(kv_heads)) if heads > 0 => embedding * kv_heads / heads,
        _ => embedding,
    };
    // The output layer is offloadable in addition to the transformer blocks.
    let layers = u32::try_from(blocks + 1)
        .map_err(|_| LlmError::Response(format!("implausible block count {blocks}")))?;

    Ok(ModelProfile {
        name: model.to_string(),
        weights_mib: size_bytes.div_ceil(BYTES_PER_MIB),
        layers,
        cache_bytes_per_token: blocks * cached_width * CACHE_BYTES_PER_DIMENSION,
    })
}

/// Sums `size_vram` over the models listed by `/api/ps`.
pub fn parse_loaded_vram_mib(running: &Value) -> u64 {
    let bytes: u64 = running
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("size_vram").and_then(Value::as_u64))
        .sum();
    bytes / BYTES_PER_MIB
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Role;

    fn tags() -> Value {
        json!({ "models": [
            { "name": "qwen2.5:14b-instruct", "size": 9 * 1024 * BYTES_PER_MIB },
            { "name": "llama3:latest", "size": 4096 * BYTES_PER_MIB + 1 },
        ]})
    }

    #[test]
    fn builds_a_profile_from_tags_and_show() {
        let show = json!({ "model_info": {
            "qwen2.block_count": 48,
            "qwen2.embedding_length": 5120,
            "qwen2.attention.head_count": 40,
            "qwen2.attention.head_count_kv": 8,
        }});

        let profile = parse_model_profile("qwen2.5:14b-instruct", &tags(), &show).unwrap();

        assert_eq!(profile.weights_mib, 9216);
        assert_eq!(profile.layers, 49);
        // 48 blocks x (5120 x 8 / 40) cached dimensions x 4 bytes.
        assert_eq!(profile.cache_bytes_per_token, 196_608);
    }

    #[test]
    fn matches_a_model_named_without_its_default_tag() {
        let show = json!({ "model_info": { "llama.block_count": 32, "llama.embedding_length": 4096 } });

        let profile = parse_model_profile("llama3", &tags(), &show).unwrap();

        assert_eq!(profile.weights_mib, 4097);
        assert_eq!(profile.cache_bytes_per_token, 32 * 4096 * 4);
    }

    #[test]
    fn reports_a_model_that_is_not_installed() {
        let result = parse_model_profile("missing", &tags(), &json!({}));

        assert!(matches!(result, Err(LlmError::UnknownModel(name)) if name == "missing"));
    }

    #[test]
    fn reports_a_model_without_a_block_count() {
        let result = parse_model_profile("llama3", &tags(), &json!({ "model_info": {} }));

        assert!(matches!(result, Err(LlmError::Response(_))));
    }

    #[test]
    fn sends_the_gpu_layer_count_as_an_option() {
        let messages = [Message::new(Role::User, "hi")];
        let request = ChatRequest {
            model: "m",
            messages: &messages,
            tools: &[],
            gpu_layers: Some(12),
            context_tokens: Some(8192),
        };

        let body = chat_body(&request);

        assert_eq!(body["options"]["num_gpu"], 12);
        assert_eq!(body["options"]["num_ctx"], 8192);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn leaves_gpu_placement_to_the_server_when_unset() {
        let request = ChatRequest {
            model: "m",
            messages: &[],
            tools: &[],
            gpu_layers: None,
            context_tokens: None,
        };

        assert_eq!(chat_body(&request)["options"], json!({ "temperature": 0.0 }));
    }

    #[test]
    fn sums_the_vram_of_loaded_models() {
        let running = json!({ "models": [
            { "size_vram": 3 * BYTES_PER_MIB },
            { "size_vram": 5 * BYTES_PER_MIB },
            { "name": "cpu-only" },
        ]});

        assert_eq!(parse_loaded_vram_mib(&running), 8);
        assert_eq!(parse_loaded_vram_mib(&json!({})), 0);
    }

    #[test]
    fn describes_tools_as_functions() {
        let tool = ToolSpec {
            name: "read_file",
            description: "Reads a file",
            parameters: json!({ "type": "object" }),
        };

        let value = tool_json(&tool);

        assert_eq!(value["type"], "function");
        assert_eq!(value["function"]["name"], "read_file");
    }

    #[test]
    fn reads_a_reply_carrying_tool_calls() {
        let raw = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "function": { "name": "run_command", "arguments": { "command": "ls" } } }],
        });

        let message: Message = serde_json::from_value(raw).unwrap();

        assert_eq!(message.tool_calls[0].function.name, "run_command");
        assert_eq!(message.tool_calls[0].function.arguments["command"], "ls");
    }
}
