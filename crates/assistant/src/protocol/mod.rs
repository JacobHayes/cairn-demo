//! The wire protocols (ARCHITECTURE, Assistant): each translates an [`Exchange`] into one
//! provider's request body and its answer back into a [`Reply`], and [`HttpProvider`] sends
//! it as one JSON POST. A deployment configures one protocol, endpoint, model, and
//! credential; the credential is configuration, held in memory and sent only in the
//! request's headers, never stored or logged.

pub mod anthropic;
pub mod chat;
pub mod openai;

use std::fmt;

use serde_json::{Value, json};
use tracing::Instrument;
use url::Url;

use crate::limits::PROVIDER_RESPONSE_BYTES_MAX;
use crate::provider::{Answer, Exchange, Protocol, Provider, ProviderError, Reply};

/// A deployment-wide API key (ARCHITECTURE, Assistant): configuration, never stored in the
/// database, and never shown: its `Debug` prints no part of it.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential(String);

impl Credential {
    /// The key as configured.
    #[must_use]
    pub fn new(key: String) -> Self {
        Self(key)
    }

    /// The key, for a request header.
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(..)")
    }
}

/// The assistant's provider configuration: the one knob it has (PRACTICES, No configuration
/// by default names it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The wire protocol.
    pub protocol: Protocol,
    /// The API's base URL, which the protocol's path is appended to: for example
    /// `https://api.anthropic.com/v1`, `https://api.openai.com/v1`, or an OpenAI-compatible
    /// server's `/v1`.
    pub endpoint: Url,
    /// The model to ask, as the provider names it.
    pub model: String,
    /// The API key; an OpenAI-compatible server may need none.
    pub credential: Option<Credential>,
}

/// A provider over HTTPS speaking one of the [`Protocol`]s.
pub struct HttpProvider {
    config: ProviderConfig,
    client: reqwest::Client,
}

impl HttpProvider {
    /// The provider `config` describes.
    #[must_use]
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// The URL a call is posted to.
    fn url(&self) -> String {
        let path = match self.config.protocol {
            Protocol::AnthropicMessages => anthropic::PATH,
            Protocol::OpenaiResponses => openai::PATH,
            Protocol::ChatCompletions => chat::PATH,
        };
        format!(
            "{}/{path}",
            self.config.endpoint.as_str().trim_end_matches('/')
        )
    }

    /// One call: the request built, posted, and its answer read.
    async fn call(&self, exchange: &Exchange) -> Result<Reply, ProviderError> {
        let model = &self.config.model;
        let key = self.config.credential.as_ref().map(Credential::expose);
        let (mut body, headers) = match self.config.protocol {
            Protocol::AnthropicMessages => {
                (anthropic::request(model, exchange), anthropic::headers(key))
            }
            Protocol::OpenaiResponses => (openai::request(model, exchange), bearer(key)),
            Protocol::ChatCompletions => (chat::request(model, exchange), bearer(key)),
        };
        chat::include_usage(&mut body, self.config.protocol, &self.config.endpoint);
        let url = self.url();
        let host = self.config.endpoint.host_str().unwrap_or("unknown");
        let http = tracing::info_span!("provider POST", sentry.op = "http.client",
            sentry.name = %format!("POST {host}{}", url::Url::parse(&url).map(|url| url.path().to_owned()).unwrap_or_default()),
            http.request.method = "POST", url.full = %url, server.address = host,
            http.response.status_code = tracing::field::Empty, sentry.status = tracing::field::Empty);
        async {
            let mut request = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .body(body.to_string());
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = request.send().await.map_err(transport)?;
            let status = response.status();
            tracing::Span::current().record("http.response.status_code", status.as_u16());
            tracing::Span::current().record(
                "sentry.status",
                if status.is_success() {
                    "ok"
                } else {
                    "internal_error"
                },
            );
            let bytes = read_bounded(response, PROVIDER_RESPONSE_BYTES_MAX).await?;
            let answer: Value =
                serde_json::from_slice(&bytes).map_err(|error| ProviderError::Failed {
                    message: format!("status {status}: the answer is not JSON: {error}"),
                })?;
            if !status.is_success() {
                return Err(ProviderError::Failed {
                    message: format!("status {status}: {}", error_message(&answer)),
                });
            }
            match self.config.protocol {
                Protocol::AnthropicMessages => anthropic::reply(&answer),
                Protocol::OpenaiResponses => openai::reply(&answer),
                Protocol::ChatCompletions => chat::reply(&answer),
            }
        }
        .instrument(http)
        .await
    }
}

impl Provider for HttpProvider {
    fn send<'a>(&'a self, exchange: &'a Exchange) -> Answer<'a> {
        let provider = match self.config.protocol {
            Protocol::AnthropicMessages => "anthropic",
            Protocol::OpenaiResponses => "openai",
            Protocol::ChatCompletions => self.config.endpoint.host_str().unwrap_or("openai"),
        };
        let span = tracing::info_span!("model call", sentry.op = "gen_ai.chat",
            sentry.name = %format!("chat {}", self.config.model),
            gen_ai.operation.name = "chat", gen_ai.provider.name = provider,
            gen_ai.request.model = %self.config.model,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.total_tokens = tracing::field::Empty,
            gen_ai.cost.total_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
            gen_ai.usage.reasoning.output_tokens = tracing::field::Empty,
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.system_instructions = %exchange.system,
            gen_ai.input.messages = %messages(exchange),
            gen_ai.output.messages = tracing::field::Empty,
            sentry.status = tracing::field::Empty);
        Box::pin(async move {
            let span = tracing::Span::current();
            if self.config.protocol == Protocol::AnthropicMessages {
                span.record("gen_ai.request.max_tokens", anthropic::REPLY_TOKEN_COUNT_MAX);
            }
            let answer = self.call(exchange).await;
            match &answer {
                Ok(reply) => {
                    if let Some(model) = &reply.model { span.record("gen_ai.response.model", model.as_str()); }
                    let output = json!([{"role": "assistant", "content": reply.text,
                        "tool_calls": reply.calls.iter().map(|call| json!({"id":call.id,"name":call.name,"arguments":call.arguments})).collect::<Vec<_>>()}]);
                    span.record("gen_ai.output.messages", output.to_string());
                    span.record("sentry.status", "ok");
                    if let Some(usage) = &reply.usage {
                        record_usage(&span, usage, &self.config.model);
                    }
                }
                Err(error) => {
                    span.record("sentry.status", "internal_error");
                    tracing::error!(%error, "model call failed");
                }
            }
            answer
        }.instrument(span))
    }
}

/// The body of `response`, refused once it is past `bytes_max` bytes, read no further.
async fn read_bounded(
    mut response: reqwest::Response,
    bytes_max: u32,
) -> Result<Vec<u8>, ProviderError> {
    let limit = usize::try_from(bytes_max).unwrap_or(usize::MAX);
    let too_large = || ProviderError::Failed {
        message: format!(
            "the answer is over {bytes_max} bytes (assistant_provider_response_bytes)"
        ),
    };
    let announced = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok());
    if announced.is_some_and(|length| length > limit) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The bearer authorization header, when there is a key.
fn bearer(key: Option<&str>) -> Vec<(&'static str, String)> {
    key.map(|key| ("authorization", format!("Bearer {key}")))
        .into_iter()
        .collect()
}

/// A transport failure, which names the URL but never a header.
fn transport(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        return ProviderError::TimedOut;
    }
    ProviderError::Failed {
        message: error.without_url().to_string(),
    }
}

/// The message of a provider's error body: every protocol here puts one at `error.message`
/// (or a bare `error` string); otherwise the body, cut short.
fn error_message(answer: &Value) -> String {
    let error = answer.get("error");
    let message = error
        .and_then(|error| error.get("message"))
        .or(error)
        .and_then(Value::as_str);
    match message {
        Some(message) => message.to_owned(),
        None => answer.to_string().chars().take(512).collect(),
    }
}

/// A protocol's answer that is not what the protocol says it sends.
pub(crate) fn malformed(what: &str) -> ProviderError {
    ProviderError::Failed {
        message: format!("the answer is not the protocol's: {what}"),
    }
}

/// A call's arguments, which these protocols send as JSON text: parsed, or kept as the
/// string when the model wrote something else, so the tool refuses it by name.
pub(crate) fn arguments_from_text(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// A call's arguments as the JSON text these protocols send.
pub(crate) fn arguments_as_text(arguments: &Value) -> String {
    arguments.to_string()
}

/// Provider usage is optional: absent counters remain absent, rather than invented zeros.
fn usage(answer: &Value, anthropic: bool) -> Option<crate::provider::Usage> {
    let usage = answer.get("usage")?;
    let mut input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))?
        .as_u64()?;
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))?
        .as_u64()?;
    if anthropic {
        input = input.checked_add(
            usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )?;
        input = input.checked_add(
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )?;
    }
    let total = match usage.get("total_tokens").and_then(Value::as_u64) {
        Some(total) => total,
        None => input.checked_add(output)?,
    };
    Some(crate::provider::Usage {
        input,
        output,
        total,
        cost_usd: usage
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0),
        cache_read_input: usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
            })
            .or_else(|| {
                usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
            }),
        cache_creation_input: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning_output: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                usage
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
            }),
    })
}

fn messages(exchange: &Exchange) -> Value {
    use crate::provider::Message;
    Value::Array(exchange.messages.iter().map(|message| match message {
        Message::User { text } => json!({"role": "user", "content": text}),
        Message::Assistant(reply) => json!({"role": "assistant", "content": reply.text,
            "tool_calls": reply.calls.iter().map(|call| json!({"id":call.id,"name":call.name,"arguments":call.arguments})).collect::<Vec<_>>()}),
        Message::ToolResults(results) => json!({"role":"tool", "content":results.iter().map(|result| json!({"tool_call_id":result.call_id,"content":result.content,"is_error":result.is_error})).collect::<Vec<_>>()}),
    }).collect())
}

/// I5 and Non-functional: Observable: records actual model usage without inventing values.
fn record_usage(span: &tracing::Span, usage: &crate::provider::Usage, model: &str) {
    span.record("gen_ai.usage.input_tokens", usage.input);
    span.record("gen_ai.usage.output_tokens", usage.output);
    span.record("gen_ai.usage.total_tokens", usage.total);
    if let Some(cost) = usage.cost_usd {
        span.record("gen_ai.cost.total_tokens", cost);
    }
    for (name, value) in [
        (
            "gen_ai.usage.cache_read.input_tokens",
            usage.cache_read_input,
        ),
        (
            "gen_ai.usage.cache_creation.input_tokens",
            usage.cache_creation_input,
        ),
        (
            "gen_ai.usage.reasoning.output_tokens",
            usage.reasoning_output,
        ),
    ] {
        if let Some(value) = value {
            span.record(name, value);
        }
    }
    metrics::counter!("cairn_assistant_tokens_total", "model" => model.to_owned(), "direction" => "input").increment(usage.input);
    metrics::counter!("cairn_assistant_tokens_total", "model" => model.to_owned(), "direction" => "output").increment(usage.output);
}
