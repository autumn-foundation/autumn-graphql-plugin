//! `GET {path}/ws` — subscriptions (and any operation) over a WebSocket.
//!
//! Both sub-protocols async-graphql implements are offered:
//! `graphql-transport-ws` (the maintained `graphql-ws` library's protocol)
//! and the legacy `graphql-ws` (Apollo `subscriptions-transport-ws`).
//!
//! Operational guarantees on top of the protocol:
//!
//! - every operation passes the same [`Pipeline`](crate::pipeline::Pipeline)
//!   as HTTP (limits, persisted queries, introspection, masking);
//! - the upgrade request runs the app's context hooks, so cookie/header
//!   authentication works; the `connection_init` payload goes to the
//!   [`WsInitHook`](crate::context::WsInitHook) for token-in-payload auth;
//! - a client that never sends `connection_init` is closed with `4408`
//!   after `init_timeout_secs`;
//! - connections beyond `max_connections` are refused with `503` before
//!   the upgrade;
//! - on shutdown every socket is closed with `1001 Going Away`, so clients
//!   reconnect to a healthy instance instead of hanging.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ::http::request::Parts;
use ::http::{StatusCode, header};
use async_graphql::http::{ALL_WEBSOCKET_PROTOCOLS, WebSocket, WebSocketProtocols, WsMessage};
use async_graphql::{Data, Executor};
use autumn_web::AppState;
use axum::extract::ws::{CloseFrame, Message, WebSocket as Socket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};

use super::{Endpoint, Format, StreamSlot, drained, rejection_response};
use crate::context::Transport;
use crate::error::{IntoGraphqlError, codes};
use crate::pipeline::{GuardedExecutor, Rejection};

/// Close code for a client that did not initialise in time.
pub const CLOSE_INIT_TIMEOUT: u16 = 4408;
/// Close code for "going away" (server shutdown).
pub const CLOSE_GOING_AWAY: u16 = 1001;
/// Close code for a refused `connection_init` (`graphql-transport-ws`).
pub const CLOSE_FORBIDDEN: u16 = 4403;
/// A close frame's reason may carry at most 123 bytes.
const MAX_CLOSE_REASON: usize = 123;

pub async fn upgrade<E: Executor>(
    State(state): State<AppState>,
    Extension(endpoint): Extension<Arc<Endpoint<E>>>,
    mut parts: Parts,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => {
            return rejection_response(
                Rejection::new(
                    codes::BAD_REQUEST,
                    format!("expected a WebSocket upgrade: {}", rejection.body_text()),
                    rejection.status(),
                ),
                Format::Json,
            );
        }
    };

    let Some(protocol) = select_protocol(&parts) else {
        return rejection_response(
            Rejection::new(
                codes::BAD_REQUEST,
                "Sec-WebSocket-Protocol must offer `graphql-transport-ws` or `graphql-ws`",
                StatusCode::BAD_REQUEST,
            ),
            Format::Json,
        );
    };

    let data = match endpoint
        .context_data(&mut parts, &state, Transport::WebSocket)
        .await
    {
        Ok(data) => data,
        Err(rejection) => return rejection_response(rejection, Format::Json),
    };

    let Some(slot) = endpoint.try_stream_slot(Transport::WebSocket) else {
        return rejection_response(
            Rejection::new(
                codes::TOO_MANY_CONNECTIONS,
                "too many concurrent connections; retry later",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            Format::Json,
        );
    };

    upgrade
        .protocols(ALL_WEBSOCKET_PROTOCOLS)
        .on_upgrade(move |socket| serve(socket, endpoint, state, protocol, data, slot))
}

/// The first protocol the client offers that async-graphql speaks.
fn select_protocol(parts: &Parts) -> Option<WebSocketProtocols> {
    parts
        .headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|p| p.trim().parse::<WebSocketProtocols>().ok())
}

#[derive(serde::Deserialize)]
struct MessageType<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
}

async fn serve<E: Executor>(
    socket: Socket,
    endpoint: Arc<Endpoint<E>>,
    state: AppState,
    protocol: WebSocketProtocols,
    data: Data,
    slot: StreamSlot,
) {
    let _slot = slot;
    let (mut sink, source) = socket.split();

    let initialised = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&initialised);
    let input = source
        .take_while(|message| {
            std::future::ready(!matches!(message, Err(_) | Ok(Message::Close(_))))
        })
        .filter_map(move |message| {
            let bytes = match message {
                Ok(Message::Text(text)) => Some(text.as_str().as_bytes().to_vec()),
                Ok(Message::Binary(bytes)) => Some(bytes.to_vec()),
                _ => None,
            };
            if let Some(bytes) = &bytes
                && serde_json::from_slice::<MessageType<'_>>(bytes)
                    .is_ok_and(|m| m.kind == "connection_init")
            {
                seen.store(true, Ordering::Release);
            }
            std::future::ready(bytes)
        });

    let executor = GuardedExecutor {
        inner: endpoint.executor.clone(),
        pipeline: Arc::clone(&endpoint.pipeline),
        transport: Transport::WebSocket,
    };
    let init_hook = endpoint.ws_init.clone();
    let mut protocol_stream = WebSocket::new(executor, input, protocol)
        .connection_data(data)
        .keepalive_timeout(endpoint.keepalive)
        .on_connection_init(move |payload| async move {
            match init_hook {
                None => Ok(Data::default()),
                Some(hook) => hook
                    .on_init(payload, state)
                    .await
                    .map_err(|e| async_graphql::Error::new(e.into_graphql_error().message)),
            }
        });

    let init_deadline = tokio::time::sleep(endpoint.ws_init_timeout);
    tokio::pin!(init_deadline);
    let stopped = drained(endpoint.shutdown.subscribe());
    tokio::pin!(stopped);

    let mut acknowledged = false;
    loop {
        tokio::select! {
            outgoing = protocol_stream.next() => match outgoing {
                Some(WsMessage::Text(text)) => {
                    acknowledged = acknowledged || is_ack(&text);
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(WsMessage::Close(code, reason)) => {
                    // async-graphql closes a refused `connection_init` with
                    // 1002; graphql-transport-ws specifies 4403 Forbidden.
                    let code = if code == 1002
                        && !acknowledged
                        && protocol == WebSocketProtocols::GraphQLWS
                    {
                        CLOSE_FORBIDDEN
                    } else {
                        code
                    };
                    let _ = sink.send(close(code, reason)).await;
                    break;
                }
                None => break,
            },
            () = &mut init_deadline, if !initialised.load(Ordering::Acquire) => {
                let _ = sink
                    .send(close(CLOSE_INIT_TIMEOUT, "Connection initialisation timeout".into()))
                    .await;
                break;
            }
            () = &mut stopped => {
                let _ = sink.send(close(CLOSE_GOING_AWAY, "Server shutting down".into())).await;
                break;
            }
        }
    }
    let _ = sink.close().await;
}

fn is_ack(text: &str) -> bool {
    serde_json::from_str::<MessageType<'_>>(text).is_ok_and(|m| m.kind == "connection_ack")
}

fn close(code: u16, mut reason: String) -> Message {
    if reason.len() > MAX_CLOSE_REASON {
        let mut cut = MAX_CLOSE_REASON;
        while !reason.is_char_boundary(cut) {
            cut -= 1;
        }
        reason.truncate(cut);
    }
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_reasons_fit_the_frame_limit() {
        let Message::Close(Some(frame)) = close(4403, "é".repeat(100)) else {
            unreachable!("close builds a close frame");
        };
        assert!(frame.reason.len() <= MAX_CLOSE_REASON);
        assert!(frame.reason.as_str().chars().all(|c| c == 'é'));
        assert!(is_ack(r#"{"type":"connection_ack"}"#));
        assert!(!is_ack(r#"{"type":"next"}"#));
    }
}
