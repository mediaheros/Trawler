//! OpenAI-compatible chat client (Ollama's /v1 endpoint).
//! Big cloud models can take a minute per turn — dedicated client, long timeout.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String, // system | user | assistant | tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMsg {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(s.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(s.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn assistant_text(s: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Some(s.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn tool_result(call_id: &str, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default = "default_call_id")]
    pub id: String,
    #[serde(rename = "type", default = "default_call_type")]
    pub call_type: String,
    pub function: FnCall,
}

fn default_call_id() -> String {
    // sentinel; chat() assigns a unique per-index id after parsing, because
    // two id-less parallel calls colliding on one id corrupts the tool loop
    String::new()
}
fn default_call_type() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FnCall {
    pub name: String,
    /// JSON-encoded arguments string per the OpenAI wire format
    pub arguments: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMsg,
}

pub struct LlmClient {
    http: reqwest::Client,
    pub base_url: String,
    pub model: String,
}

impl LlmClient {
    pub fn new(base_url: &str, model: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("llm http client");
        Self { http, base_url: base_url.trim_end_matches('/').to_string(), model: model.to_string() }
    }

    pub async fn chat(&self, messages: &[ChatMsg], tools: Option<&Value>) -> Result<ChatMsg> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Other(format!("agent model unreachable: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Other(format!(
                "agent model error {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            )));
        }
        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Other(format!("agent model returned malformed response: {e}")))?;
        let mut msg = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| AppError::Other("agent model returned no choices".into()))?;
        // Ollama's shim sometimes omits tool-call ids; give each a unique one
        // so parallel calls can't collide when results are matched back
        if let Some(calls) = msg.tool_calls.as_mut() {
            for (i, call) in calls.iter_mut().enumerate() {
                if call.id.is_empty() {
                    call.id = format!("call_{i}");
                }
            }
        }
        Ok(msg)
    }

    /// List models from Ollama's native API (richer than /v1/models).
    pub async fn models(&self) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| AppError::Other(format!("agent endpoint unreachable: {e}")))?;
        let v: Value = resp.json().await.map_err(|e| AppError::Other(e.to_string()))?;
        Ok(v.get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }
}
