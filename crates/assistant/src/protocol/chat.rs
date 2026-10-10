//! A generic OpenAI-compatible chat-completions endpoint (`OpenRouter`, self-hosted models):
//! `POST {endpoint}/chat/completions`, with a bearer key when one is configured. The system
//! prompt is the first message; a tool call is an entry of the assistant message's
//! `tool_calls` with its arguments as JSON text, and its result a `tool` message. Servers
//! differ in what else they put on a message, so a reply is sent back rebuilt from Cairn's
//! types rather than as it came.

use serde_json::{Value, json};

use super::{arguments_as_text, arguments_from_text, malformed};
use crate::provider::{Exchange, Message, ProviderError, Reply, ToolCall};

/// The path under the endpoint.
pub const PATH: &str = "chat/completions";

/// The request body for `exchange`.
#[must_use]
pub fn request(model: &str, exchange: &Exchange) -> Value {
    let tools: Vec<Value> = exchange
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect();
    let mut messages = vec![json!({ "role": "system", "content": exchange.system })];
    for message in &exchange.messages {
        entries(message, &mut messages);
    }
    json!({
        "model": model,
        "messages": messages,
        "tools": tools,
    })
}

/// The chat messages of one message.
fn entries(message: &Message, into: &mut Vec<Value>) {
    match message {
        Message::User { text } => into.push(json!({ "role": "user", "content": text })),
        Message::Assistant(reply) => {
            let mut entry = json!({ "role": "assistant", "content": reply.text });
            if !reply.calls.is_empty() {
                let calls: Vec<Value> = reply
                    .calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": arguments_as_text(&call.arguments),
                            },
                        })
                    })
                    .collect();
                entry["tool_calls"] = Value::Array(calls);
            }
            into.push(entry);
        }
        Message::ToolResults(results) => into.extend(results.iter().map(|result| {
            json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": result.content,
            })
        })),
    }
}

/// The reply in an answer: the first choice's message.
///
/// # Errors
///
/// [`ProviderError::Failed`] when there is no choice with a message, or a call is malformed.
pub fn reply(answer: &Value) -> Result<Reply, ProviderError> {
    let message = answer
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| malformed("no choice with a message"))?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let mut calls = Vec::new();
    let listed = message.get("tool_calls").and_then(Value::as_array);
    for call in listed.into_iter().flatten() {
        let function = call.get("function");
        let field = |of: Option<&Value>, name: &str| {
            of.and_then(|of| of.get(name))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let (Some(id), Some(name), Some(arguments)) = (
            field(Some(call), "id"),
            field(function, "name"),
            field(function, "arguments"),
        ) else {
            return Err(malformed("a tool call without its id, name, or arguments"));
        };
        calls.push(ToolCall {
            id,
            name,
            arguments: arguments_from_text(&arguments),
        });
    }
    Ok(Reply {
        model: answer
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage: super::usage(answer, false),
        text,
        calls,
        raw: None,
    })
}

/// I5: `OpenRouter` usage accounting is an extension, sent only to its endpoint.
pub(super) fn include_usage(
    body: &mut Value,
    protocol: crate::provider::Protocol,
    endpoint: &url::Url,
) {
    if protocol == crate::provider::Protocol::ChatCompletions
        && endpoint.host_str() == Some("openrouter.ai")
    {
        body["usage"] = json!({"include": true});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Protocol;

    /// I5: endpoint-specific extensions never reach another provider or protocol.
    #[test]
    fn accounting_is_requested_only_for_openrouter_chat() {
        for (endpoint, protocol, expected) in [
            (
                "https://openrouter.ai/api/v1",
                Protocol::ChatCompletions,
                true,
            ),
            (
                "https://openrouter.ai.other/api/v1",
                Protocol::ChatCompletions,
                false,
            ),
            (
                "https://api.openai.com/v1",
                Protocol::ChatCompletions,
                false,
            ),
            (
                "https://openrouter.ai/api/v1",
                Protocol::OpenaiResponses,
                false,
            ),
        ] {
            let mut body = json!({"model": "reference-model"});
            include_usage(&mut body, protocol, &endpoint.parse().unwrap());
            assert_eq!(
                body.get("usage"),
                expected.then(|| json!({"include": true})).as_ref()
            );
        }
    }

    /// I5: real provider accounting includes cost and subsets without changing totals.
    #[test]
    fn optional_usage_preserves_cost_cached_and_reasoning_tokens() {
        let mut answer = json!({"choices": [{"message": {"content": "Ready"}}],
            "usage": {"prompt_tokens": 31, "completion_tokens": 7, "total_tokens": 38,
            "cost": 0.00123, "prompt_tokens_details": {"cached_tokens": 5},
            "completion_tokens_details": {"reasoning_tokens": 3}}});
        let usage = reply(&answer).unwrap().usage.unwrap();
        assert_eq!((usage.input, usage.output, usage.total), (31, 7, 38));
        assert_eq!(usage.cost_usd, Some(0.00123));
        assert_eq!(usage.cache_read_input, Some(5));
        assert_eq!(usage.reasoning_output, Some(3));
        answer["usage"]["cost"] = json!(-1);
        answer["usage"]["prompt_tokens_details"]["cached_tokens"] = json!("invalid");
        let usage = reply(&answer).unwrap().usage.unwrap();
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.cache_read_input, None);
        answer.as_object_mut().unwrap().remove("usage");
        assert!(reply(&answer).unwrap().usage.is_none());
    }
}
