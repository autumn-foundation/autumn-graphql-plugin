//! GraphQL over Server-Sent Events, "distinct connections" mode.
//!
//! A client that sends `Accept: text/event-stream` (on `POST` or `GET`)
//! gets one `next` event per result and a final `complete` event. Queries
//! and mutations produce one `next`; subscriptions stream until the source
//! ends, the client disconnects, or the server shuts down. SSE rides plain
//! HTTP, so it passes through proxies, CSRF protection and the endpoint's
//! guard layer exactly like any other request — the reason to prefer it
//! over `WebSockets` behind strict infrastructure.

use std::convert::Infallible;
use std::sync::Arc;

use ::http::request::Parts;
use ::http::{Method, StatusCode};
use async_graphql::Executor;
use autumn_web::AppState;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};

use super::http::Incoming;
use super::{Endpoint, Format, drained, rejection_response};
use crate::context::Transport;
use crate::error::codes;
use crate::limits;
use crate::pipeline::Rejection;
use async_graphql::parser::types::OperationType;

pub async fn serve<E: Executor>(
    endpoint: Arc<Endpoint<E>>,
    state: AppState,
    mut parts: Parts,
    incoming: Incoming,
) -> Response {
    let Incoming {
        mut request,
        document_id,
    } = incoming;
    if let Err(rejection) = endpoint
        .attach(&mut request, &mut parts, &state, Transport::Sse)
        .await
    {
        return rejection_response(rejection, Format::Json);
    }
    // Persisted documents are resolved up front so the stream below sees a
    // plain document; everything else runs inside `execute_stream`.
    if let Some(id) = document_id
        && let Err(rejection) = endpoint
            .pipeline
            .prepare(&mut request, Some(&id), Transport::Sse)
            .await
    {
        return rejection_response(rejection, Format::Json);
    }
    // `GET` is what caches, prefetchers and cross-site links replay: over
    // SSE it may carry queries and subscriptions (browsers' `EventSource`
    // can only `GET`), never a mutation.
    let operation_name = request.operation_name.clone();
    if parts.method == Method::GET
        && let Ok(document) = request.parsed_query()
        && limits::operation_type(document, operation_name.as_deref())
            == Some(OperationType::Mutation)
    {
        return rejection_response(
            Rejection::new(
                codes::METHOD_NOT_ALLOWED,
                "mutation operations are not allowed over GET; use POST",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            Format::Json,
        );
    }
    let Some(slot) = endpoint.try_stream_slot(Transport::Sse) else {
        return rejection_response(
            Rejection::new(
                codes::TOO_MANY_CONNECTIONS,
                "too many concurrent streams; retry later",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            Format::Json,
        );
    };

    let stopped = drained(endpoint.shutdown.subscribe());

    let results = endpoint
        .pipeline
        .execute_stream(&endpoint.executor, request, None, Transport::Sse)
        .map(|response| {
            Ok::<_, Infallible>(
                Event::default()
                    .event("next")
                    .json_data(&response)
                    .unwrap_or_else(|_| Event::default().event("next").data("{}")),
            )
        });
    let complete = stream::once(async move {
        // Held until the stream is dropped, so the slot and the gauge track
        // the real lifetime of the connection.
        drop(slot);
        Ok::<_, Infallible>(Event::default().event("complete").data(""))
    });
    let events = results.take_until(Box::pin(stopped)).chain(complete);

    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(endpoint.keepalive))
        .into_response()
}
