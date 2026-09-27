//! The request pipeline every transport shares.
//!
//! ```text
//! prepare ─┬─ persisted-query resolution (APQ / trusted documents)
//!          ├─ text limits (size, lexical nesting)      ← before the parser
//!          ├─ parse (once; the parsed document is reused by the executor)
//!          ├─ transport policy (no mutations over GET)
//!          ├─ operation limits (depth, aliases, fields, …)
//!          └─ introspection policy
//! execute ──  the schema (with a timeout, for request/response transports)
//! finish  ─┬─ error codes + masking of unexpected errors
//!          └─ metrics + slow-operation log
//! ```
//!
//! HTTP handlers call [`Pipeline::execute_one`]; the WebSocket and SSE
//! transports go through [`GuardedExecutor`], which runs the same steps
//! inside async-graphql's [`Executor`] contract. No transport can reach the
//! schema without passing `prepare`.

use std::sync::Arc;
use std::time::Duration;

use async_graphql::parser::types::OperationType;
use async_graphql::{Data, Executor, IntrospectionMode, Request, Response, Value};
use futures_util::stream::{self, BoxStream, StreamExt};
use http::StatusCode;
use tokio::time::Instant;
use tracing::Instrument;

use crate::config::{LimitsConfig, PersistedQueryMode};
use crate::context::Transport;
use crate::error::{codes, finalize_errors, request_error};
use crate::limits::{self, AnalysisError, LimitViolation};
use crate::metrics::Metrics;
use crate::persisted::{PersistedQueryStore, TrustedDocuments, is_sha256_hex, sha256_hex};

/// Effective, context-resolved settings the pipeline runs with.
#[derive(Debug, Clone)]
pub struct PipelineSettings {
    /// Allow `__schema`/`__type`.
    pub introspection: bool,
    /// Mask uncoded resolver errors.
    pub mask_unexpected_errors: bool,
    /// Per-operation timeout for request/response transports.
    pub timeout: Option<Duration>,
    /// Log operations slower than this.
    pub slow_operation: Option<Duration>,
    /// Static-analysis limits.
    pub limits: LimitsConfig,
    /// Persisted-query mode.
    pub persisted_mode: PersistedQueryMode,
}

/// A request refused before execution.
#[derive(Debug, Clone)]
pub struct Rejection {
    /// `extensions.code`.
    pub code: &'static str,
    /// Client-facing message.
    pub message: String,
    /// HTTP status under `application/graphql-response+json`.
    pub status: StatusCode,
    /// Whether the status applies under `application/json` too (a
    /// transport-level refusal such as `405`).
    pub transport_level: bool,
    /// Extra `extensions` entries.
    pub extra: Vec<(&'static str, Value)>,
}

impl Rejection {
    pub(crate) fn new(code: &'static str, message: impl Into<String>, status: StatusCode) -> Self {
        Self {
            code,
            message: message.into(),
            status,
            transport_level: false,
            extra: Vec::new(),
        }
    }

    pub(crate) const fn transport_level(mut self) -> Self {
        self.transport_level = true;
        self
    }

    fn limit(violation: LimitViolation) -> Self {
        let status = if violation.limit == "query_bytes" {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        };
        let max = i64::try_from(violation.max).unwrap_or(i64::MAX);
        let actual = i64::try_from(violation.actual).unwrap_or(i64::MAX);
        Self {
            code: codes::QUERY_LIMIT_EXCEEDED,
            message: violation.to_string(),
            status,
            transport_level: false,
            extra: vec![
                ("limit", Value::from(violation.limit)),
                ("max", Value::from(max)),
                ("actual", Value::from(actual)),
            ],
        }
    }

    /// The rejection as a GraphQL response (no `data`, one coded error).
    #[must_use]
    pub fn into_response(self) -> Response {
        Response::from_errors(vec![request_error(
            self.code,
            self.message,
            self.status,
            &self.extra,
        )])
    }
}

/// What `prepare` learned about an operation.
#[derive(Debug, Clone)]
pub struct OperationInfo {
    /// `operationName`, if the request named one.
    pub name: Option<String>,
    /// The operation's type, when the document parsed.
    pub kind: Option<OperationType>,
    /// Whether the document parsed.
    pub parsed: bool,
    /// Short hash of the document, for logs.
    pub document_hash: String,
    /// The transport that carried it.
    pub transport: Transport,
    /// When preparation started.
    pub started: Instant,
}

/// Label for an operation type.
#[must_use]
pub const fn kind_label(kind: Option<OperationType>) -> &'static str {
    match kind {
        Some(OperationType::Query) => "query",
        Some(OperationType::Mutation) => "mutation",
        Some(OperationType::Subscription) => "subscription",
        None => "unknown",
    }
}

/// An executed (or refused) request/response operation.
pub struct Executed {
    /// The GraphQL response.
    pub response: Response,
    /// The HTTP status under `application/graphql-response+json`.
    pub status: StatusCode,
    /// A status that applies whatever the media type (e.g. `405`).
    pub transport_status: Option<StatusCode>,
    /// The operation's type, if known.
    pub kind: Option<OperationType>,
}

/// Shared per-endpoint pipeline state.
pub struct Pipeline {
    pub(crate) settings: PipelineSettings,
    pub(crate) store: Arc<dyn PersistedQueryStore>,
    pub(crate) trusted: Arc<TrustedDocuments>,
    pub(crate) metrics: Metrics,
}

impl Pipeline {
    /// Resolve persisted queries and enforce every pre-execution policy.
    ///
    /// # Errors
    ///
    /// Returns the [`Rejection`] to send instead of executing.
    pub async fn prepare(
        &self,
        request: &mut Request,
        document_id: Option<&str>,
        transport: Transport,
    ) -> Result<OperationInfo, Rejection> {
        let started = Instant::now();
        self.resolve_persisted(request, document_id).await?;
        limits::check_text(&request.query, &self.settings.limits).map_err(Rejection::limit)?;

        let name = request.operation_name.clone();
        let mut info = OperationInfo {
            name,
            kind: None,
            parsed: false,
            document_hash: sha256_hex(&request.query).chars().take(16).collect(),
            transport,
            started,
        };

        // Parse once; async-graphql reuses the parsed document. A syntax
        // error is left for the executor, which reports it in the standard
        // shape (and `finish` codes it GRAPHQL_PARSE_FAILED).
        let Ok(document) = request.parsed_query() else {
            return Ok(info);
        };
        info.parsed = true;
        info.kind = limits::operation_type(document, info.name.as_deref());

        if transport == Transport::HttpGet
            && matches!(
                info.kind,
                Some(OperationType::Mutation | OperationType::Subscription)
            )
        {
            return Err(Rejection::new(
                codes::METHOD_NOT_ALLOWED,
                format!(
                    "{} operations are not allowed over GET; use POST",
                    kind_label(info.kind)
                ),
                StatusCode::METHOD_NOT_ALLOWED,
            )
            .transport_level());
        }

        match limits::analyze(document, info.name.as_deref()) {
            Ok(stats) => {
                if stats.introspection && !self.settings.introspection {
                    return Err(Rejection::new(
                        codes::INTROSPECTION_DISABLED,
                        "introspection is disabled on this endpoint",
                        StatusCode::BAD_REQUEST,
                    ));
                }
                limits::check_stats(&stats, &self.settings.limits).map_err(Rejection::limit)?;
            }
            Err(AnalysisError::TooComplex) => {
                return Err(Rejection::new(
                    codes::QUERY_LIMIT_EXCEEDED,
                    "document is nested too deeply to analyse",
                    StatusCode::BAD_REQUEST,
                ));
            }
            // Cycles and a missing/ambiguous operation are validation errors
            // the executor reports with its own, better messages.
            Err(AnalysisError::FragmentCycle(_) | AnalysisError::NoOperation) => {}
        }

        // Defence in depth: documents the analyzer passes through (fragment
        // cycles, unselectable operations) still cannot introspect.
        if !self.settings.introspection {
            request.introspection_mode = IntrospectionMode::Disabled;
        }
        Ok(info)
    }

    async fn resolve_persisted(
        &self,
        request: &mut Request,
        document_id: Option<&str>,
    ) -> Result<(), Rejection> {
        let apq_hash = apq_hash(request)?;
        let id = document_id
            .map(|id| id.strip_prefix("sha256:").unwrap_or(id).to_owned())
            .or(apq_hash);
        let has_query = !request.query.trim().is_empty();
        let mode = self.settings.persisted_mode;

        match (mode, id) {
            (PersistedQueryMode::Disabled, Some(_)) if !has_query => Err(Rejection::new(
                codes::PERSISTED_QUERY_NOT_SUPPORTED,
                "PersistedQueryNotSupported",
                StatusCode::BAD_REQUEST,
            )),
            (PersistedQueryMode::Disabled, _) | (PersistedQueryMode::Automatic, None) => Ok(()),

            (PersistedQueryMode::Automatic, Some(id)) if has_query => {
                let trusted_match = self
                    .trusted
                    .get(&id)
                    .is_some_and(|doc| **doc == *request.query);
                if !trusted_match && sha256_hex(&request.query) != id.to_ascii_lowercase() {
                    return Err(Rejection::new(
                        codes::PERSISTED_QUERY_HASH_MISMATCH,
                        "provided sha does not match query",
                        StatusCode::BAD_REQUEST,
                    ));
                }
                if !trusted_match {
                    self.store
                        .put(&id.to_ascii_lowercase(), Arc::from(request.query.as_str()))
                        .await;
                }
                Ok(())
            }
            (PersistedQueryMode::Automatic, Some(id)) => {
                let found = match self.trusted.get(&id) {
                    Some(doc) => Some(Arc::clone(doc)),
                    None => self.store.get(&id.to_ascii_lowercase()).await,
                };
                found.map_or_else(
                    || Err(not_found()),
                    |doc| {
                        request.query = doc.to_string();
                        Ok(())
                    },
                )
            }

            (PersistedQueryMode::Trusted, Some(id)) => match self.trusted.get(&id) {
                Some(doc) if !has_query => {
                    request.query = doc.to_string();
                    Ok(())
                }
                Some(doc) if **doc == *request.query => Ok(()),
                Some(_) => Err(not_allowlisted()),
                None if has_query => Err(not_allowlisted()),
                None => Err(not_found()),
            },
            (PersistedQueryMode::Trusted, None) if !has_query => Ok(()),
            (PersistedQueryMode::Trusted, None) => {
                if self.trusted.contains_document(&request.query) {
                    Ok(())
                } else {
                    Err(not_allowlisted())
                }
            }
        }
    }

    /// Code errors, mask, and record metrics for a finished response.
    pub fn finish(&self, response: &mut Response, info: &OperationInfo) {
        let error_codes = finalize_errors(
            &mut response.errors,
            self.settings.mask_unexpected_errors,
            info.parsed,
        );
        let elapsed = info.started.elapsed();
        let outcome = if response.errors.is_empty() {
            "success"
        } else {
            "error"
        };
        self.metrics
            .operation(info.transport, info.kind, outcome, Some(elapsed));
        for code in &error_codes {
            self.metrics.error(code);
        }
        if let Some(slow) = self.settings.slow_operation
            && elapsed >= slow
        {
            tracing::warn!(
                operation.name = info.name.as_deref().unwrap_or("<anonymous>"),
                operation.kind = kind_label(info.kind),
                document = %info.document_hash,
                elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                "slow GraphQL operation"
            );
        }
    }

    /// Record and render a rejection.
    pub fn reject(&self, rejection: Rejection, transport: Transport) -> Response {
        tracing::debug!(code = rejection.code, message = %rejection.message, "GraphQL request rejected");
        self.metrics.operation(transport, None, "rejected", None);
        self.metrics.error(rejection.code);
        rejection.into_response()
    }

    /// Prepare, execute (with the timeout) and finish one request/response
    /// operation.
    pub async fn execute_one<E: Executor>(
        &self,
        executor: &E,
        mut request: Request,
        document_id: Option<&str>,
        transport: Transport,
    ) -> Executed {
        let info = match self.prepare(&mut request, document_id, transport).await {
            Ok(info) => info,
            Err(rejection) => {
                let status = rejection.status;
                let transport_status = rejection.transport_level.then_some(status);
                return Executed {
                    response: self.reject(rejection, transport),
                    status,
                    transport_status,
                    kind: None,
                };
            }
        };

        let span = tracing::info_span!(
            "graphql.operation",
            otel.name = %format!("{} {}", kind_label(info.kind), info.name.as_deref().unwrap_or("")).trim_end(),
            graphql.operation.name = info.name.as_deref().unwrap_or(""),
            graphql.operation.type = kind_label(info.kind),
            graphql.document = %info.document_hash,
            transport = transport.as_str(),
        );
        let run = executor.execute(request).instrument(span);
        #[allow(clippy::option_if_let_else)] // two awaits read clearer as a match
        let (mut response, timed_out) = match self.settings.timeout {
            Some(limit) => match tokio::time::timeout(limit, run).await {
                Ok(response) => (response, false),
                Err(_) => (
                    Response::from_errors(vec![request_error(
                        codes::OPERATION_TIMEOUT,
                        format!("operation did not complete within {}ms", limit.as_millis()),
                        StatusCode::GATEWAY_TIMEOUT,
                        &[],
                    )]),
                    true,
                ),
            },
            None => (run.await, false),
        };
        self.finish(&mut response, &info);

        let status = if timed_out {
            StatusCode::GATEWAY_TIMEOUT
        } else if is_request_error(&response) {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        };
        Executed {
            response,
            status,
            transport_status: None,
            kind: info.kind,
        }
    }

    /// Prepare and start a streaming operation (SSE and WebSocket). Every
    /// item is finished like a single response.
    pub fn execute_stream<E: Executor>(
        self: &Arc<Self>,
        executor: &E,
        request: Request,
        session_data: Option<Arc<Data>>,
        transport: Transport,
    ) -> BoxStream<'static, Response> {
        let pipeline = Arc::clone(self);
        let executor = executor.clone();
        stream::once(async move {
            let mut request = request;
            match pipeline.prepare(&mut request, None, transport).await {
                Err(rejection) => {
                    stream::once(std::future::ready(pipeline.reject(rejection, transport))).boxed()
                }
                Ok(info) => {
                    pipeline
                        .metrics
                        .operation(transport, info.kind, "started", None);
                    executor
                        .execute_stream(request, session_data)
                        .map(move |mut response| {
                            let codes = finalize_errors(
                                &mut response.errors,
                                pipeline.settings.mask_unexpected_errors,
                                info.parsed,
                            );
                            for code in &codes {
                                pipeline.metrics.error(code);
                            }
                            response
                        })
                        .boxed()
                }
            }
        })
        .flatten()
        .boxed()
    }
}

/// A GraphQL-over-HTTP "request error": nothing executed (no `data`) and
/// every error is a pre-execution one.
#[must_use]
pub fn is_request_error(response: &Response) -> bool {
    matches!(response.data, Value::Null)
        && !response.errors.is_empty()
        && response.errors.iter().all(|e| e.path.is_empty())
}

fn not_found() -> Rejection {
    Rejection::new(
        codes::PERSISTED_QUERY_NOT_FOUND,
        "PersistedQueryNotFound",
        StatusCode::NOT_FOUND,
    )
}

fn not_allowlisted() -> Rejection {
    Rejection::new(
        codes::OPERATION_NOT_ALLOWLISTED,
        "this operation is not in the trusted-documents manifest",
        StatusCode::FORBIDDEN,
    )
}

/// Extract `extensions.persistedQuery.sha256Hash`, validating the shape.
fn apq_hash(request: &Request) -> Result<Option<String>, Rejection> {
    let Some(value) = request.extensions.get("persistedQuery") else {
        return Ok(None);
    };
    let bad = |message: &str| Rejection::new(codes::BAD_REQUEST, message, StatusCode::BAD_REQUEST);
    let Value::Object(object) = value else {
        return Err(bad("extensions.persistedQuery must be an object"));
    };
    match object.get("version") {
        Some(Value::Number(n)) if n.as_i64() == Some(1) => {}
        _ => return Err(bad("unsupported persisted query version")),
    }
    match object.get("sha256Hash") {
        Some(Value::String(hash)) if is_sha256_hex(hash) => Ok(Some(hash.to_ascii_lowercase())),
        _ => Err(bad(
            "extensions.persistedQuery.sha256Hash must be a SHA-256 hex digest",
        )),
    }
}

/// An [`Executor`] that runs the pipeline in front of `inner`.
///
/// The WebSocket and SSE transports hand this to async-graphql, so subscriptions
/// get exactly the same limits, persisted-query rules, introspection policy
/// and error masking as HTTP.
#[derive(Clone)]
pub struct GuardedExecutor<E> {
    pub(crate) inner: E,
    pub(crate) pipeline: Arc<Pipeline>,
    pub(crate) transport: Transport,
}

#[cfg(not(feature = "boxed-trait"))]
impl<E: Executor> Executor for GuardedExecutor<E> {
    fn execute(&self, request: Request) -> impl std::future::Future<Output = Response> + Send {
        let this = self.clone();
        async move {
            this.pipeline
                .execute_one(&this.inner, request, None, this.transport)
                .await
                .response
        }
    }

    fn execute_stream(
        &self,
        request: Request,
        session_data: Option<Arc<Data>>,
    ) -> BoxStream<'static, Response> {
        self.pipeline
            .execute_stream(&self.inner, request, session_data, self.transport)
    }
}

#[cfg(feature = "boxed-trait")]
#[async_trait::async_trait]
impl<E: Executor> Executor for GuardedExecutor<E> {
    async fn execute(&self, request: Request) -> Response {
        self.pipeline
            .execute_one(&self.inner, request, None, self.transport)
            .await
            .response
    }

    fn execute_stream(
        &self,
        request: Request,
        session_data: Option<Arc<Data>>,
    ) -> BoxStream<'static, Response> {
        self.pipeline
            .execute_stream(&self.inner, request, session_data, self.transport)
    }
}
