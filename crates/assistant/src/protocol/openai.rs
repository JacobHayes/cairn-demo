//! The `OpenAI` Responses API: `POST {endpoint}/responses` with a bearer key. The system
//! prompt is `instructions`; the conversation is a list of input items, a tool call a
//! `function_call` item with its arguments as JSON text, and its result a
//! `function_call_output` item. Nothing is stored at the provider (`store: false`), so the
//! reasoning items of a reply are asked for encrypted and sent back unchanged within a turn.

use serde_json::{Value, json};

use super::{arguments_as_text, arguments_from_text, malformed};
use crate::provider::{Exchange, Message, ProviderError, Reply, ToolCall};

/// The path under the endpoint.
pub const PATH: &str = "responses";

/// The request body for `exchange`.
#[must_use]
pub fn request(model: &str, exchange: &Exchange) -> Value {
    let tools: Vec<Value> = exchange
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        })
        .collect();
    let mut input = Vec::new();
    for message in &exchange.messages {
        items(message, &mut input);
    }
    json!({
        "model": model,
        "instructions": exchange.system,
        "input": input,
        "tools": tools,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    })
}

/// The input items of one message.
fn items(message: &Message, into: &mut Vec<Value>) {
    match message {
        Message::User { text } => into.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })),
        Message::Assistant(reply) => {
            if let Some(Value::Array(output)) = &reply.raw {
                into.extend(output.iter().cloned());
                return;
            }
            into.extend(reply.text.iter().map(|text| {
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                })
            }));
            into.extend(reply.calls.iter().map(|call| {
                json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": arguments_as_text(&call.arguments),
                })
            }));
        }
        Message::ToolResults(results) => into.extend(results.iter().map(|result| {
            json!({
                "type": "function_call_output",
                "call_id": result.call_id,
                "output": result.content,
            })
        })),
    }
}

/// The reply in an answer: the text of its `message` items, its `function_call` items as
/// calls.
///
/// # Errors
///
/// [`ProviderError::Failed`] when the response failed or has no output list, or an item is
/// malformed.
pub fn reply(answer: &Value) -> Result<Reply, ProviderError> {
    if answer.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(ProviderError::Failed {
            message: super::error_message(answer),
        });
    }
    let Some(output) = answer.get("output").and_then(Value::as_array) else {
        return Err(malformed("no output list"));
    };
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for item in output {
        let field = |name: &str| item.get(name).and_then(Value::as_str);
        match field("type") {
            Some("message") => {
                let content = item.get("content").and_then(Value::as_array);
                let parts = content
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"));
                for part in parts {
                    let text = part.get("text").and_then(Value::as_str);
                    texts.push(text.ok_or_else(|| malformed("output text without text"))?);
                }
            }
            Some("function_call") => {
                let (Some(id), Some(name), Some(arguments)) =
                    (field("call_id"), field("name"), field("arguments"))
                else {
                    return Err(malformed(
                        "a function call without its id, name, or arguments",
                    ));
                };
                calls.push(ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: arguments_from_text(arguments),
                });
            }
            // Reasoning and items Cairn does not read ride back in `raw`.
            _ => {}
        }
    }
    let text = (!texts.is_empty()).then(|| texts.concat());
    Ok(Reply {
        model: answer
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage: super::usage(answer, false),
        text,
        calls,
        raw: Some(Value::Array(output.clone())),
    })
}
