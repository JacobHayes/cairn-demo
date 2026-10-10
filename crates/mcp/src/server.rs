//! The MCP server (ARCHITECTURE, MCP endpoint; I2): rmcp over Streamable HTTP, serving the
//! tool set, the guide as its instructions, and each workflow as a prompt (I4). It is a
//! plain axum router the API mounts behind its auth layer, which hands every request its
//! actor; the server never authenticates on its own.
//!
//! Stateless: every request is a POST answered with one JSON response, no session is kept
//! between requests, and no stream stays open, so each call is held to the API's request
//! limits like any other request: its duration, its body (the same `request_bytes_max`), and
//! the requests in flight
//! (decisions/2026-10-06-the-mcp-endpoint-is-stateless-json-inside-the-apis-router.md).

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::http::request::Parts;
use cairn_schema::limits::REQUEST_BYTES_MAX;
use cairn_schema::{Actor, Rejection};
use cairn_store::Store;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult, ListToolsResult,
    PaginatedRequestParams, Prompt, PromptMessage, Role, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value};
use tracing::Instrument;

use crate::{ToolError, ToolSet, instructions, tools};

/// Where the endpoint is served (I2).
pub const MCP_PATH: &str = "/api/mcp";

/// The MCP endpoint over `tools` at [`MCP_PATH`], for a router whose auth layer puts an
/// [`Actor`] in every request's extensions.
pub fn router<S: Store + 'static>(tools: ToolSet<S>) -> Router {
    // Host and Origin checks guard a server nothing else authenticates; this one sits
    // behind the auth layer, and its public host is the deployment's, which the listener
    // owns
    // (decisions/2026-10-06-the-mcp-endpoint-is-stateless-json-inside-the-apis-router.md).
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(usize::try_from(REQUEST_BYTES_MAX).unwrap_or(usize::MAX))
        .disable_allowed_hosts();
    let server = Server { tools };
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new().route_service(MCP_PATH, service)
}

/// The rmcp handler: one per request, sharing the tool set.
struct Server<S> {
    tools: ToolSet<S>,
}

impl<S> Clone for Server<S> {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
        }
    }
}

impl<S: Store + 'static> ServerHandler for Server<S> {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("cairn", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions::guide())
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        let tools = ToolSet::<S>::definitions().into_iter().map(|definition| {
            let destructive = tools::spec(definition.name).is_some_and(|spec| spec.destructive);
            let mut annotations = ToolAnnotations::new()
                .read_only(!definition.writes)
                .destructive(destructive);
            // A write resubmitted under its patch id is answered from its receipt (H5).
            annotations.idempotent_hint = Some(definition.writes);
            // Output schemas stay out of the list: an agent reads the output it gets, and they
            // would multiply the list's size, per
            // decisions/2026-10-06-the-tool-list-carries-the-mutation-vocabulary-once-and-no.md.
            Tool::new(
                definition.name,
                definition.description,
                Arc::new(definition.input_schema),
            )
            .with_annotations(annotations)
        });
        std::future::ready(Ok(ListToolsResult::with_all_items(tools.collect())))
            .instrument(request_span(&context, "tools/list", None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let span = request_span(&context, "tools/call", Some(request.name.as_ref()));
        async {
            let actor = actor(&context)?;
            let name = request.name.to_string();
            let arguments = Value::Object(request.arguments.unwrap_or_default());
            let answer = observed(self.tools.clone(), actor, name, arguments).await;
            tracing::Span::current().record("mcp.tool.result.is_error", answer.is_err());
            tracing::Span::current().record(
                "sentry.status",
                if answer.is_ok() {
                    "ok"
                } else {
                    "internal_error"
                },
            );
            let result = match answer {
                Ok(output) => CallToolResult::structured(output),
                Err(error @ ToolError::UnknownTool { .. }) => {
                    return Err(ErrorData::invalid_params(error.to_string(), None));
                }
                Err(error @ ToolError::Failed { .. }) => {
                    return Err(ErrorData::internal_error(error.to_string(), None));
                }
                Err(error) => refused(&error),
            };
            Ok(result.into())
        }
        .instrument(span.clone())
        .await
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        let prompts = instructions::workflows()
            .into_iter()
            .map(|workflow| Prompt::new(workflow.name, Some(workflow.description), None));
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts.collect())))
            .instrument(request_span(&context, "prompts/list", None))
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResponse, ErrorData>> + Send + '_ {
        let workflow = instructions::workflows()
            .into_iter()
            .find(|workflow| workflow.name == request.name);
        let answer = if let Some(workflow) = workflow {
            let message = PromptMessage::new(Role::User, ContentBlock::text(workflow.text));
            let result = GetPromptResult::new(vec![message]);
            Ok(result.with_description(workflow.description).into())
        } else {
            let message = format!("no prompt is named {}", request.name);
            Err(ErrorData::invalid_params(message, None))
        };
        std::future::ready(answer).instrument(request_span(&context, "prompts/get", None))
    }
}

/// Tool calls answered, by tool and outcome (ARCHITECTURE, Observability): `ok`, or what
/// refused it (`invalid`, `stale`, `patch_id_reused`, `arguments`, `not_found`, `refused`,
/// `unknown_tool`, `failed`).
pub const TOOL_CALLS: &str = "cairn_mcp_tool_calls_total";
/// Tool call latency in seconds, by tool.
pub const TOOL_CALL_DURATION: &str = "cairn_mcp_tool_call_duration_seconds";
/// The tool label of a call naming no tool.
pub const UNKNOWN_TOOL: &str = "unknown";

/// Runs one tool call, logged, counted, and timed by tool. A write runs on its own task,
/// so the request duration limit can answer the agent without cutting it off between its
/// commit and its announcement (H6), and a resubmission by patch id finds what it did (H5);
/// a read runs in the request and stops with it. A name no tool has is counted under one
/// label, so callers cannot mint metric series.
async fn observed<S: Store + 'static>(
    tools: ToolSet<S>,
    actor: Actor,
    name: String,
    arguments: Value,
) -> Result<Value, ToolError> {
    let Some(spec) = crate::tools::spec(&name) else {
        metrics::counter!(TOOL_CALLS, "tool" => UNKNOWN_TOOL, "outcome" => "unknown_tool")
            .increment(1);
        return Err(ToolError::UnknownTool { name });
    };
    let tool = spec.name;
    let patch_id = arguments
        .get("patch_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let span = tracing::info_span!(
        "tool",
        tool,
        patch_id = patch_id.as_deref(),
        mcp.tool.result.is_error = tracing::field::Empty,
        sentry.status = tracing::field::Empty
    );
    async {
        let started = Instant::now();
        let answer = if spec.writes {
            let task = tokio::spawn(
                async move { tools.call(&actor, tool, arguments).await }.in_current_span(),
            );
            task.await.unwrap_or_else(|error| {
                Err(ToolError::Failed {
                    message: format!("the tool's task failed: {error}"),
                })
            })
        } else {
            tools.call(&actor, tool, arguments).await
        };
        let elapsed = started.elapsed();
        let outcome = outcome(&answer);
        span.in_scope(|| {
            tracing::info!(outcome, duration_ms = elapsed.as_millis(), "answered");
        });
        metrics::counter!(TOOL_CALLS, "tool" => tool, "outcome" => outcome).increment(1);
        metrics::histogram!(TOOL_CALL_DURATION, "tool" => tool).record(elapsed.as_secs_f64());
        let failed = answer.is_err();
        tracing::Span::current().record("mcp.tool.result.is_error", failed);
        tracing::Span::current().record(
            "sentry.status",
            if failed { "internal_error" } else { "ok" },
        );
        if let Err(ToolError::Failed { message }) = &answer {
            tracing::error!(%message, "MCP tool failed");
        }
        answer
    }
    .instrument(span.clone())
    .await
}

/// A call's outcome, as the count names it.
fn outcome(answer: &Result<Value, ToolError>) -> &'static str {
    match answer {
        Ok(_) => "ok",
        Err(ToolError::Rejected { rejection }) => match rejection {
            Rejection::Invalid { .. } => "invalid",
            Rejection::Stale { .. } => "stale",
            Rejection::PatchIdReused { .. } => "patch_id_reused",
        },
        Err(ToolError::Arguments { .. }) => "arguments",
        Err(ToolError::NotFound { .. }) => "not_found",
        Err(ToolError::Refused { .. }) => "refused",
        Err(ToolError::UnknownTool { .. }) => "unknown_tool",
        Err(ToolError::Failed { .. }) => "failed",
    }
}

/// The actor the auth layer put on the request.
fn actor(context: &RequestContext<RoleServer>) -> Result<Actor, ErrorData> {
    let parts = context.extensions.get::<Parts>();
    let actor = parts.and_then(|parts| parts.extensions.get::<Actor>());
    actor
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("the request reached MCP without an actor", None))
}

/// A tool error the agent can act on, as the tool's result: its message, and the error as
/// structured content (a rejection carries every violation, A15).
fn refused(error: &ToolError) -> CallToolResult {
    let structured = match serde_json::to_value(error) {
        Ok(value) => value,
        Err(failure) => Value::Object(Map::from_iter([(
            "error".to_owned(),
            Value::String(failure.to_string()),
        )])),
    };
    let mut result = CallToolResult::structured_error(structured);
    result
        .content
        .insert(0, ContentBlock::text(error.to_string()));
    result
}

fn request_span(
    context: &RequestContext<RoleServer>,
    method: &str,
    tool: Option<&str>,
) -> tracing::Span {
    let client = context.peer.peer_info();
    let user_agent = context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.headers.get(axum::http::header::USER_AGENT))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_whitespace().next());
    let (client_name, client_version) = user_agent.map_or_else(
        || {
            client.as_ref().map_or(("unknown", "unknown"), |info| {
                (
                    info.client_info.name.as_str(),
                    info.client_info.version.as_str(),
                )
            })
        },
        |agent| agent.split_once('/').unwrap_or((agent, "unknown")),
    );
    let name = tool.map_or_else(|| method.to_owned(), |tool| format!("{method} {tool}"));
    tracing::info_span!("MCP request", sentry.op = "mcp.server", sentry.name = %name,
        mcp.method.name = method, mcp.tool.name = tool, gen_ai.tool.name = tool,
        mcp.transport = "http", network.transport = "tcp", mcp.request.id = %context.id,
        mcp.client.name = client_name,
        mcp.client.version = client_version,
        mcp.tool.result.is_error = tracing::field::Empty, sentry.status = tracing::field::Empty)
}
