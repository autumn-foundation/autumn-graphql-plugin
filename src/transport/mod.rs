//! HTTP, WebSocket and SSE transports over one [`Pipeline`].

pub mod http;
pub mod sse;
pub mod ws;

use std::sync::Arc;
use std::time::Duration;

use ::http::request::Parts;
use ::http::{HeaderValue, StatusCode, header};
use async_graphql::{Data, Request};
use autumn_web::{AppState, AutumnError};
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{BatchingConfig, UploadsConfig};
use crate::context::{ContextHook, GraphqlRequestInfo, Transport, WsInitHook, run_hooks};
use crate::error::IntoGraphqlError;
use crate::metrics::Metrics;
use crate::pipeline::{Pipeline, Rejection};

/// Everything one mounted endpoint needs at request time. Carried on the
/// nested router as an `Extension`, so two endpoints never share state even
/// when they are built from the same schema type.
pub struct Endpoint<E> {
    pub executor: E,
    pub pipeline: Arc<Pipeline>,
    pub hooks: Vec<Arc<dyn ContextHook>>,
    pub ws_init: Option<Arc<dyn WsInitHook>>,
    pub path: Arc<str>,
    pub sdl: Option<Arc<str>>,
    pub allow_get: bool,
    pub max_body_bytes: usize,
    pub batching: BatchingConfig,
    pub uploads: UploadsConfig,
    pub csrf_prevention: bool,
    pub sse: bool,
    pub keepalive: Duration,
    pub ws_init_timeout: Duration,
    pub streams: Option<Arc<Semaphore>>,
    pub shutdown: crate::plugin::DrainHandle,
    pub metrics: Metrics,
}

impl<E: async_graphql::Executor> Endpoint<E> {
    /// Build the context data one operation starts with: `AppState`, the
    /// request info, then every app hook in order.
    pub(crate) async fn context_data(
        &self,
        parts: &mut Parts,
        state: &AppState,
        transport: Transport,
    ) -> Result<Data, Rejection> {
        let mut data = Data::default();
        data.insert(state.clone());
        data.insert(GraphqlRequestInfo::from_parts(parts, transport, &self.path));
        run_hooks(&self.hooks, parts, state, &mut data)
            .await
            .map_err(hook_rejection)?;
        Ok(data)
    }

    /// Attach per-request data to a request. Requests are always built by
    /// this crate's transports with empty data, so replacing is merging.
    pub(crate) async fn attach(
        &self,
        request: &mut Request,
        parts: &mut Parts,
        state: &AppState,
        transport: Transport,
    ) -> Result<(), Rejection> {
        request.data = self.context_data(parts, state, transport).await?;
        Ok(())
    }

    /// Take a stream slot, or `None` at the connection limit.
    pub(crate) fn try_stream_slot(&self, transport: Transport) -> Option<StreamSlot> {
        let permit = match &self.streams {
            Some(semaphore) => Some(Arc::clone(semaphore).try_acquire_owned().ok()?),
            None => None,
        };
        self.metrics.stream_opened(transport);
        Some(StreamSlot {
            _permit: permit,
            metrics: self.metrics.clone(),
            transport,
        })
    }
}

/// A held connection slot; releases the permit and the gauge on drop.
pub struct StreamSlot {
    _permit: Option<OwnedSemaphorePermit>,
    metrics: Metrics,
    transport: Transport,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.metrics.stream_closed(self.transport);
    }
}

/// A context hook refused the request: keep its status (a `401` stays a
/// `401`) and render it GraphQL-shaped, redacted like any resolver error.
fn hook_rejection(error: AutumnError) -> Rejection {
    let status = error.status();
    let converted = error.into_graphql_error();
    let code = converted
        .extensions
        .as_ref()
        .and_then(|e| match e.get("code") {
            Some(async_graphql::Value::String(code)) => Some(code.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let mut rejection = Rejection::new(
        static_code(&code),
        converted.message,
        if status.is_client_error() || status.is_server_error() {
            status
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        },
    )
    .transport_level();
    if let Some(ext) = converted.extensions.as_ref()
        && let Some(fields) = ext.get("fields")
    {
        rejection.extra.push(("fields", fields.clone()));
    }
    rejection
}

/// Map a dynamic code back to its `'static` constant.
fn static_code(code: &str) -> &'static str {
    use crate::error::codes;
    [
        codes::BAD_USER_INPUT,
        codes::UNAUTHENTICATED,
        codes::FORBIDDEN,
        codes::NOT_FOUND,
        codes::CONFLICT,
        codes::VALIDATION_FAILED,
        codes::RATE_LIMITED,
        codes::CLIENT_ERROR,
        codes::SERVICE_UNAVAILABLE,
    ]
    .into_iter()
    .find(|known| *known == code)
    .unwrap_or(codes::INTERNAL_SERVER_ERROR)
}

/// Response media types this endpoint can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `application/graphql-response+json` — status codes carry meaning.
    GraphqlResponseJson,
    /// `application/json` — always `200` for GraphQL-level errors.
    Json,
    /// `text/event-stream`.
    EventStream,
}

impl Format {
    pub(crate) const fn content_type(self) -> &'static str {
        match self {
            Self::GraphqlResponseJson => "application/graphql-response+json; charset=utf-8",
            Self::Json => "application/json; charset=utf-8",
            Self::EventStream => "text/event-stream",
        }
    }
}

/// Negotiate the response media type from `Accept`.
///
/// - No `Accept` → `application/json` (the spec's legacy-client rule).
/// - Explicit types beat wildcards; `q=0` excludes.
/// - Between two equally-preferred explicit JSON types,
///   `application/graphql-response+json` wins; a bare wildcard yields
///   `application/json`, so `curl` and older clients keep getting `200`s.
/// - `text/event-stream` is only chosen when named explicitly and SSE is on.
///
/// Returns `None` when nothing acceptable remains (→ `406`).
pub fn negotiate(accept: Option<&str>, sse: bool) -> Option<Format> {
    let Some(accept) = accept.map(str::trim).filter(|a| !a.is_empty()) else {
        return Some(Format::Json);
    };
    let ranges: Vec<(String, f32)> = accept
        .split(',')
        .filter_map(|range| {
            let mut pieces = range.split(';');
            let media = pieces.next()?.trim().to_ascii_lowercase();
            let q = pieces
                .filter_map(|p| p.trim().strip_prefix("q=").map(str::trim))
                .find_map(|q| q.parse::<f32>().ok())
                .unwrap_or(1.0);
            (!media.is_empty()).then_some((media, q))
        })
        .collect();

    // (q, specificity) for a candidate: the most specific matching range
    // decides its q, as RFC 9110 prescribes.
    let score = |candidate: &str, allow_wildcard: bool| -> Option<(f32, u8)> {
        let (kind, _) = candidate.split_once('/')?;
        ranges
            .iter()
            .filter_map(|(media, q)| {
                let specificity = if media == candidate {
                    2
                } else if allow_wildcard && media == &format!("{kind}/*") {
                    1
                } else if allow_wildcard && media == "*/*" {
                    0
                } else {
                    return None;
                };
                Some((*q, specificity))
            })
            .max_by_key(|(_, specificity)| *specificity)
            .filter(|(q, _)| *q > 0.0)
    };

    let mut best: Option<(Format, f32, u8)> = None;
    let candidates = [
        (
            Format::GraphqlResponseJson,
            "application/graphql-response+json",
            true,
        ),
        (Format::Json, "application/json", true),
        (Format::EventStream, "text/event-stream", false),
    ];
    for (format, media, wildcard) in candidates {
        if format == Format::EventStream && !sse {
            continue;
        }
        let Some((q, specificity)) = score(media, wildcard) else {
            continue;
        };
        let better = match best {
            None => true,
            Some((current, bq, bs)) => {
                q > bq
                    || ((q - bq).abs() < f32::EPSILON && specificity > bs)
                    || ((q - bq).abs() < f32::EPSILON
                        && specificity == bs
                        && specificity < 2
                        && format == Format::Json
                        && current == Format::GraphqlResponseJson)
            }
        };
        if better {
            best = Some((format, q, specificity));
        }
    }
    best.map(|(format, _, _)| format)
}

/// Render a GraphQL-shaped body with a status and media type.
pub fn json_response(status: StatusCode, format: Format, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let content_type = if format == Format::EventStream {
        Format::Json.content_type()
    } else {
        format.content_type()
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// Render a rejection as a full HTTP response (used before a pipeline run,
/// for transport-level refusals).
pub fn rejection_response(rejection: Rejection, format: Format) -> Response {
    let status = rejection.status;
    let body = serde_json::to_vec(&rejection.into_response()).unwrap_or_default();
    json_response(status, format, body)
}

/// A bare `4xx` for requests that are not GraphQL at all.
pub fn plain(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

/// Resolves on the first drain after `rx` subscribed. The handle's sender
/// lives as long as the endpoint, so a closed channel cannot happen in
/// practice; if it ever did, never resolving is the safe reading.
pub async fn drained(mut rx: tokio::sync::watch::Receiver<u64>) {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_follows_the_graphql_over_http_rules() {
        let n = |accept: Option<&str>| negotiate(accept, true);
        assert_eq!(n(None), Some(Format::Json));
        assert_eq!(n(Some("")), Some(Format::Json));
        assert_eq!(n(Some("*/*")), Some(Format::Json));
        assert_eq!(n(Some("application/*")), Some(Format::Json));
        assert_eq!(n(Some("application/json")), Some(Format::Json));
        assert_eq!(
            n(Some("application/graphql-response+json")),
            Some(Format::GraphqlResponseJson)
        );
        assert_eq!(
            n(Some("application/graphql-response+json, application/json")),
            Some(Format::GraphqlResponseJson)
        );
        assert_eq!(
            n(Some(
                "application/graphql-response+json;q=0.5, application/json"
            )),
            Some(Format::Json)
        );
        assert_eq!(
            n(Some(
                "application/json;q=0.9, application/graphql-response+json"
            )),
            Some(Format::GraphqlResponseJson)
        );
        assert_eq!(
            n(Some("application/graphql-response+json, */*;q=0.1")),
            Some(Format::GraphqlResponseJson)
        );
        assert_eq!(n(Some("text/event-stream")), Some(Format::EventStream));
        assert_eq!(negotiate(Some("text/event-stream"), false), None);
        assert_eq!(n(Some("text/html")), None);
        assert_eq!(n(Some("application/json;q=0")), None);
        assert_eq!(
            n(Some("*/*, application/json;q=0")),
            Some(Format::GraphqlResponseJson)
        );
        assert_eq!(
            n(Some("text/*")),
            None,
            "text/* never selects SSE implicitly"
        );
    }

    #[test]
    fn static_codes_round_trip() {
        assert_eq!(static_code("UNAUTHENTICATED"), "UNAUTHENTICATED");
        assert_eq!(static_code("whatever"), "INTERNAL_SERVER_ERROR");
    }
}
