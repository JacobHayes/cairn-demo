//! The Anthropic Messages API: `POST {endpoint}/messages` with the key in `x-api-key` and
//! the API version in `anthropic-version`. The system prompt is its own field; a tool call
//! is a `tool_use` content block and its result a `tool_result` block in a user message.
//! The assistant's content blocks are sent back unchanged within a turn, so blocks Cairn
//! does not read (reasoning) reach the model as it wrote them.

use serde_json::{Value, json};

use super::malformed;
use crate::provider::{Exchange, Message, ProviderError, Reply, ToolCall};

/// The path under the endpoint.
pub const PATH: &str = "messages";

/// The API version the request is written against.
pub const VERSION: &str = "2023-06-01";

/// The most tokens one reply may hold, which the protocol requires a request to name: a long
/// proposal fits, and at typical output rates a reply this long still lands inside the
/// provider call limit.
pub const REPLY_TOKEN_COUNT_MAX: u32 = 8_192;

/// The headers besides the content type.
#[must_use]
pub fn headers(key: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("anthropic-version", VERSION.to_owned())];
    headers.extend(key.map(|key| ("x-api-key", key.to_owned())));
    headers
}

/// The request body for `exchange`.
#[must_use]
pub fn request(model: &str, exchange: &Exchange) -> Value {
    let tools: Vec<Value> = exchange
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect();
    let messages: Vec<Value> = exchange.messages.iter().map(message).collect();
    json!({
        "model": model,
        "max_tokens": REPLY_TOKEN_COUNT_MAX,
        "system": exchange.system,
        "messages": messages,
        "tools": tools,
    })
}

/// One message.
fn message(message: &Message) -> Value {
    match message {
        Message::User { text } => json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        }),
        Message::Assistant(reply) => {
            let content = reply.raw.clone().unwrap_or_else(|| {
                let text = reply
                    .text
                    .iter()
                    .map(|text| json!({ "type": "text", "text": text }));
                let calls = reply.calls.iter().map(|call| {
                    json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    })
                });
                Value::Array(text.chain(calls).collect())
            });
            json!({ "role": "assistant", "content": content })
        }
        Message::ToolResults(results) => {
            let content: Vec<Value> = results
                .iter()
                .map(|result| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": result.call_id,
                        "content": result.content,
                        "is_error": result.is_error,
                    })
                })
                .collect();
            json!({ "role": "user", "content": content })
        }
    }
}

/// The reply in an answer: its text blocks joined, its `tool_use` blocks as calls.
///
/// # Errors
///
/// [`ProviderError::Failed`] when the answer has no content list, or a block is malformed.
pub fn reply(answer: &Value) -> Result<Reply, ProviderError> {
    let Some(content) = answer.get("content").and_then(Value::as_array) else {
        return Err(malformed("no content list"));
    };
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block.get("text").and_then(Value::as_str);
                texts.push(text.ok_or_else(|| malformed("a text block without text"))?);
            }
            Some("tool_use") => {
                let field = |name: &str| block.get(name).and_then(Value::as_str).map(str::to_owned);
                let (Some(id), Some(name)) = (field("id"), field("name")) else {
                    return Err(malformed("a tool_use block without its id or name"));
                };
                let arguments = block.get("input").cloned().unwrap_or(Value::Null);
                calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            // Reasoning and blocks Cairn does not read ride back in `raw`.
            _ => {}
        }
    }
    let text = (!texts.is_empty()).then(|| texts.join("\n\n"));
    Ok(Reply {
        model: answer
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage: super::usage(answer, true),
        text,
        calls,
        raw: Some(Value::Array(content.clone())),
    })
}
