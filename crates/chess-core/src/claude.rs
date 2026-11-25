//! A small blocking client for the Claude Messages API.
//!
//! Rust has no official Anthropic SDK, so this talks to the HTTP API
//! directly.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::secrets::ApiKey;

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const API_VERSION: &str = "2026-10-21";
/// Lets the API retry a refused request on another model by itself.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// The three models the tutor uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Model {
    /// Turning screenshots into FENs.
    Haiku,
    /// Normal explanations.
    Sonnet,
    /// Sharp tactical positions.
    Opus,
}

impl Model {
    pub fn id(self) -> &'static str {
        match self {
            Model::Haiku => "claude-haiku-5-5",
            Model::Sonnet => "claude-sonnet-5-5",
            Model::Opus => "claude-opus-5-5",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Model::Haiku => "Haiku 5.5",
            Model::Sonnet => "Sonnet 5.5",
            Model::Opus => "Opus 5.5",
        }
    }

    /// List prices in dollars per million tokens (input, output), as of
    /// 2026-11-24, for prompts up to 100K tokens.
    pub fn prices(self) -> (f64, f64) {
        match self {
            Model::Haiku => (0.10, 0.50),
            Model::Sonnet => (2.0, 10.0),
            Model::Opus => (4.0, 20.0),
        }
    }

    fn supports_fallbacks(self) -> bool {
        !matches!(self, Model::Haiku)
    }
}

/// Effort controls thinking depth and token spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// Approximate cost in dollars at list prices. Cache reads are billed at
    /// a tenth of the input price and cache writes at 1.25x.
    pub fn cost(&self, model: Model) -> f64 {
        let (inp, out) = model.prices();
        (self.input_tokens as f64 * inp
            + self.cache_read_input_tokens as f64 * inp * 0.1
            + self.cache_creation_input_tokens as f64 * inp * 1.25
            + self.output_tokens as f64 * out)
            / 1_000_000.0
    }
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub text: String,
    pub model: Model,
    /// The model that actually answered (differs after a fallback).
    pub served_by: String,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("network error: {0}")]
    Network(String),
    #[error("Claude API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("Claude declined to answer this request")]
    Refusal,
    #[error("the answer was cut off at the token limit")]
    Truncated,
    #[error("unexpected response from the Claude API: {0}")]
    Malformed(String),
}

/// One message of a conversation; `content` is either a string or an array
/// of content blocks, exactly as the API takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: &'static str,
    pub content: Value,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message {
            role: "user",
            content: Value::String(text.into()),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Message {
        Message {
            role: "assistant",
            content: Value::String(text.into()),
        }
    }

    pub fn user_blocks(blocks: Vec<Value>) -> Message {
        Message {
            role: "user",
            content: Value::Array(blocks),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub model: Model,
    pub system: String,
    pub messages: Vec<Message>,
    pub effort: Effort,
    pub max_tokens: u32,
    /// JSON schema for structured output, if any.
    pub output_schema: Option<Value>,
}

impl Request {
    /// The JSON body sent to the API.
    pub fn body(&self) -> Value {
        let messages: Vec<Value> = self
            .messages
            .iter()
            .map(|m| json!({"role": m.role, "content": m.content}))
            .collect();
        let mut output_config = json!({"effort": self.effort.as_str()});
        if let Some(schema) = &self.output_schema {
            output_config["format"] = json!({"type": "json_schema", "schema": schema});
        }
        let mut body = json!({
            "model": self.model.id(),
            "max_tokens": self.max_tokens,
            // The system prompt is identical across requests, so cache it;
            // it pays off in long follow-up conversations.
            "system": [{
                "type": "text",
                "text": self.system,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": messages,
            "output_config": output_config,
        });
        if self.model.supports_fallbacks() {
            body["fallbacks"] = json!("default");
        }
        body
    }
}

pub struct ClaudeClient {
    key: ApiKey,
    agent: ureq::Agent,
    url: String,
}

impl std::fmt::Debug for ClaudeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeClient")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl ClaudeClient {
    pub fn new(key: ApiKey) -> ClaudeClient {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build();
        ClaudeClient {
            key,
            agent: config.into(),
            url: API_URL.to_string(),
        }
    }

    pub fn send(&self, request: &Request) -> Result<Reply, ClaudeError> {
        let body = request.body();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut call = self
                .agent
                .post(&self.url)
                .header("x-api-key", self.key.expose())
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            if request.model.supports_fallbacks() {
                call = call.header("anthropic-beta", FALLBACK_BETA);
            }
            let mut response = call
                .send_json(&body)
                .map_err(|e| ClaudeError::Network(e.to_string()))?;
            let status = response.status().as_u16();
            let value: Value = response
                .body_mut()
                .read_json()
                .map_err(|e| ClaudeError::Malformed(e.to_string()))?;
            if status == 200 {
                return parse_reply(request.model, &value);
            }
            // Retry rate limits and overloads a couple of times.
            let retryable = status == 429 || status == 529 || status >= 500;
            if retryable && attempt < 3 {
                std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
                continue;
            }
            let message = value["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            return Err(ClaudeError::Api { status, message });
        }
    }
}

/// Pulls the text and usage out of a Messages API response.
pub fn parse_reply(model: Model, value: &Value) -> Result<Reply, ClaudeError> {
    match value["stop_reason"].as_str() {
        Some("refusal") => return Err(ClaudeError::Refusal),
        Some("max_tokens") => return Err(ClaudeError::Truncated),
        _ => {}
    }
    let blocks = value["content"]
        .as_array()
        .ok_or_else(|| ClaudeError::Malformed("no content".into()))?;
    // Thinking blocks are skipped; only the visible answer is shown.
    let text: String = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let u = &value["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Ok(Reply {
        text,
        model,
        served_by: value["model"].as_str().unwrap_or(model.id()).to_string(),
        usage: Usage {
            input_tokens: n("input_tokens"),
            output_tokens: n("output_tokens"),
            cache_read_input_tokens: n("cache_read_input_tokens"),
            cache_creation_input_tokens: n("cache_creation_input_tokens"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape() {
        let req = Request {
            model: Model::Sonnet,
            system: "sys".into(),
            messages: vec![Message::user("hi")],
            effort: Effort::Low,
            max_tokens: 4000,
            output_schema: None,
        };
        let body = req.body();
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body.get("thinking").is_none());

        let haiku = Request {
            model: Model::Haiku,
            output_schema: Some(json!({"type": "object"})),
            ..req
        };
        let body = haiku.body();
        assert!(body.get("fallbacks").is_none());
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn parses_reply_and_skips_thinking() {
        let v = json!({
            "model": "claude-sonnet-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": "Nf3 develops."}],
            "usage": {"input_tokens": 2000, "output_tokens": 500}
        });
        let r = parse_reply(Model::Sonnet, &v).unwrap();
        assert_eq!(r.text, "Nf3 develops.");
        assert!((r.usage.cost(Model::Sonnet) - 0.009).abs() < 1e-9);
        let refused = json!({"stop_reason": "refusal", "content": []});
        assert!(matches!(
            parse_reply(Model::Sonnet, &refused),
            Err(ClaudeError::Refusal)
        ));
    }
}
