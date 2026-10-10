//! How the API answers what went wrong (PRACTICES, Errors, panics, and rejections). A
//! rejected patch answers the engine's or the store's `Rejection` unchanged, every
//! violation by path (A15): 422 when it is invalid, 409 when it is stale or reuses a patch
//! id. Everything else answers a [`Problem`] carrying the request id the server logged it
//! under.

use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use cairn_engine::DraftError;
use cairn_schema::Rejection;
use cairn_service::{ProposeError, ReadError, ServiceError, WriteError};
use cairn_store::StoreError;

use crate::observe;
use crate::wire::{Problem, ProblemCode};

/// Seconds a client waits before retrying an answer that names a limit.
const RETRY_AFTER_SECONDS: &str = "1";

/// An answer other than success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// A rejected patch, sent unchanged.
    Rejected(Rejection),
    /// Anything else.
    Problem {
        /// What kind of error.
        code: ProblemCode,
        /// What went wrong.
        message: String,
    },
}

impl ApiError {
    /// A problem of `code` saying `message`.
    pub fn problem(code: ProblemCode, message: impl Into<String>) -> Self {
        ApiError::Problem {
            code,
            message: message.into(),
        }
    }

    /// No such resource: `what` names it.
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::problem(ProblemCode::NotFound, format!("no {what}"))
    }

    /// A malformed request.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::problem(ProblemCode::BadRequest, message)
    }
}

/// The status a problem answers with.
#[must_use]
pub fn status_of(code: ProblemCode) -> StatusCode {
    match code {
        ProblemCode::BadRequest | ProblemCode::TargetMismatch => StatusCode::BAD_REQUEST,
        ProblemCode::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ProblemCode::CannotDraft => StatusCode::UNPROCESSABLE_ENTITY,
        ProblemCode::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ProblemCode::NotFound | ProblemCode::NoSuchEndpoint => StatusCode::NOT_FOUND,
        ProblemCode::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        ProblemCode::PageMoved => StatusCode::CONFLICT,
        ProblemCode::UserOnly => StatusCode::FORBIDDEN,
        ProblemCode::Overloaded
        | ProblemCode::SubscriberLimit
        | ProblemCode::StoreBusy
        | ProblemCode::TimedOut => StatusCode::SERVICE_UNAVAILABLE,
        ProblemCode::EnginePanic | ProblemCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The status a rejection answers with: 422 for an invalid patch, 409 for one that lost to
/// another (stale, or a reused patch id).
#[must_use]
pub fn rejection_status(rejection: &Rejection) -> StatusCode {
    match rejection {
        Rejection::Invalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        Rejection::Stale { .. } | Rejection::PatchIdReused { .. } => StatusCode::CONFLICT,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Rejected(rejection) => {
                (rejection_status(&rejection), axum::Json(rejection)).into_response()
            }
            ApiError::Problem { code, message } => {
                let body = Problem {
                    error: code,
                    message,
                    request_id: observe::current_request_id(),
                };
                let mut response = (status_of(code), axum::Json(body)).into_response();
                if status_of(code) == StatusCode::SERVICE_UNAVAILABLE {
                    let wait = HeaderValue::from_static(RETRY_AFTER_SECONDS);
                    response.headers_mut().insert(RETRY_AFTER, wait);
                }
                response
            }
        }
    }
}

impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Store(StoreError::AcquireTimeout) => {
                ApiError::problem(ProblemCode::StoreBusy, error.to_string())
            }
            ServiceError::Store(StoreError::Malformed(_) | StoreError::Backend(_)) => {
                tracing::error!(%error, "the store failed");
                ApiError::problem(ProblemCode::Internal, error.to_string())
            }
            ServiceError::SubscriberLimit(limit) => {
                ApiError::problem(ProblemCode::SubscriberLimit, limit.to_string())
            }
            ServiceError::EnginePanic {
                ref domain,
                ref message,
            } => {
                // PRACTICES, Programmer errors panic: logged with the domain and, through
                // the request's span, its id; counted; answered 500 with the request id.
                tracing::error!(target: "cairn_api::engine_panic", %domain, %message, "the engine panicked");
                metrics::counter!(observe::ENGINE_PANICS).increment(1);
                ApiError::problem(ProblemCode::EnginePanic, error.to_string())
            }
        }
    }
}

impl From<ReadError> for ApiError {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::JourneyMissing(id) => ApiError::not_found(format_args!("journey {id}")),
            ReadError::ProposalMissing(id) => ApiError::not_found(format_args!("proposal {id}")),
            // The query names a node the journey does not hold (the engine's one projection
            // error).
            ReadError::Projection(error) => {
                ApiError::problem(ProblemCode::NotFound, error.to_string())
            }
            ReadError::Failed(error) => error.into(),
        }
    }
}

impl From<WriteError> for ApiError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::Rejected(rejection) => ApiError::Rejected(rejection),
            WriteError::Failed(error) => error.into(),
        }
    }
}

impl From<ProposeError> for ApiError {
    fn from(error: ProposeError) -> Self {
        match error {
            ProposeError::Draft(DraftError::JourneyMissing(id)) => {
                ApiError::not_found(format_args!("journey {id}"))
            }
            ProposeError::Draft(DraftError::VersionMissing(lineage)) => ApiError::not_found(
                format_args!("version {} of route {}", lineage.version, lineage.route),
            ),
            ProposeError::Draft(
                error @ (DraftError::NoLineage(_)
                | DraftError::NotNewer { .. }
                | DraftError::TooLarge),
            ) => ApiError::problem(ProblemCode::CannotDraft, error.to_string()),
            ProposeError::ProposalMissing(id) => ApiError::not_found(format_args!("proposal {id}")),
            ProposeError::Write(error) => error.into(),
        }
    }
}

impl From<cairn_auth::AuthError> for ApiError {
    fn from(error: cairn_auth::AuthError) -> Self {
        match error {
            cairn_auth::AuthError::AgentNotAllowed => {
                ApiError::problem(ProblemCode::UserOnly, error.to_string())
            }
            cairn_auth::AuthError::NoSuchToken => ApiError::not_found("such agent token"),
            cairn_auth::AuthError::Store(error) => ServiceError::Store(error).into(),
            cairn_auth::AuthError::Random(_)
            | cairn_auth::AuthError::Upstream(_)
            | cairn_auth::AuthError::IdentityHeldByAnotherUser { .. } => {
                tracing::error!(%error, "auth failed");
                ApiError::problem(ProblemCode::Internal, error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_schema::Domain;

    fn answer(error: ApiError) -> (StatusCode, Option<HeaderValue>, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let retry = response.headers().get(RETRY_AFTER).cloned();
        let body = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        (status, retry, serde_json::from_slice(&body).unwrap())
    }

    #[test]
    fn an_engine_panic_answers_500_and_is_counted() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let error = ServiceError::EnginePanic {
            domain: Domain::Deployment,
            message: "an assertion failed".to_owned(),
        };
        let (status, retry, body) =
            metrics::with_local_recorder(&recorder, || answer(ApiError::from(error)));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(retry, None);
        assert_eq!(body["error"], "engine_panic");
        let counted = snapshotter.snapshot().into_vec();
        assert_eq!(counted.len(), 1);
        assert_eq!(counted[0].0.key().name(), observe::ENGINE_PANICS);
    }

    #[test]
    fn limits_answer_503_with_retry_after_and_rejections_their_own_status() {
        let busy = ApiError::from(ServiceError::Store(StoreError::AcquireTimeout));
        let (status, retry, body) = answer(busy);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(retry.is_some());
        assert_eq!(body["error"], "store_busy");

        let reused = Rejection::PatchIdReused {
            patch_id: "p_one".parse().unwrap(),
        };
        let (status, _, body) = answer(ApiError::Rejected(reused.clone()));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(serde_json::from_value::<Rejection>(body).unwrap(), reused);
    }
}
