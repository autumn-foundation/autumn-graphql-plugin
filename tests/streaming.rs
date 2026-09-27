//! WebSocket and SSE transports, over a real socket.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use autumn_plugin_graphql::{CLOSE_GOING_AWAY, CLOSE_INIT_TIMEOUT, GraphqlPlugin};
use autumn_web::test::TestApp;
use common::{AppSchema, Viewer, plugin};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Serve the plugin on an ephemeral port.
async fn serve(plugin: GraphqlPlugin<AppSchema>) -> SocketAddr {
    let router = TestApp::new().plugin(plugin).build().into_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

async fn connect(
    addr: SocketAddr,
    protocol: Option<&str>,
    user: Option<&str>,
) -> Result<Ws, WsError> {
    let mut request = format!("ws://{addr}/graphql/ws")
        .into_client_request()
        .unwrap();
    if let Some(protocol) = protocol {
        request
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
    }
    if let Some(user) = user {
        request
            .headers_mut()
            .insert("x-user", user.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(ws, _)| ws)
}

async fn send(ws: &mut Ws, message: Value) {
    ws.send(Message::text(message.to_string())).await.unwrap();
}

/// Next JSON message, or the close frame as `{"close": code}`.
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a message")
            .expect("stream ended");
        match message {
            Ok(Message::Text(text)) => return serde_json::from_str(&text).unwrap(),
            Ok(Message::Close(frame)) => {
                return json!({ "close": frame.map(|f| u16::from(f.code)), "reason": "" });
            }
            Ok(_) => {}
            Err(error) => return json!({ "error": error.to_string() }),
        }
    }
}

async fn init(ws: &mut Ws, payload: Value) {
    send(ws, json!({ "type": "connection_init", "payload": payload })).await;
    assert_eq!(recv(ws).await["type"], "connection_ack");
}

/// Collect `next` payloads until `complete` (or an `error`).
async fn collect(ws: &mut Ws, id: &str) -> (Vec<Value>, Value) {
    let mut payloads = Vec::new();
    loop {
        let message = recv(ws).await;
        match message["type"].as_str() {
            Some("next" | "data") if message["id"] == id => {
                payloads.push(message["payload"].clone());
            }
            Some("complete" | "error") if message["id"] == id => return (payloads, message),
            Some("ping" | "ka") => {}
            _ => panic!("unexpected message: {message}"),
        }
    }
}

fn with_viewer_hooks(plugin: GraphqlPlugin<AppSchema>) -> GraphqlPlugin<AppSchema> {
    plugin
        .context(|parts, _state, data| {
            Box::pin(async move {
                if let Some(user) = parts.headers.get("x-user").and_then(|v| v.to_str().ok()) {
                    data.insert(Viewer(user.to_owned()));
                }
                Ok(())
            })
        })
        .on_ws_init(|payload: Value, _state| async move {
            let mut data = async_graphql::Data::default();
            match payload["token"].as_str() {
                Some("bad") => return Err(autumn_web::AutumnError::unauthorized_msg("bad token")),
                Some(token) => data.insert(Viewer(format!("token:{token}"))),
                None => {}
            }
            Ok(data)
        })
}

#[tokio::test]
async fn graphql_transport_ws_streams_a_subscription() {
    let addr = serve(plugin()).await;
    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    init(&mut ws, json!({})).await;
    send(
        &mut ws,
        json!({ "id": "1", "type": "subscribe", "payload": { "query": "subscription { ticks(count: 3) }" } }),
    )
    .await;
    let (payloads, end) = collect(&mut ws, "1").await;
    assert_eq!(end["type"], "complete");
    let ticks: Vec<i64> = payloads
        .iter()
        .map(|p| p["data"]["ticks"].as_i64().unwrap())
        .collect();
    assert_eq!(ticks, [1, 2, 3]);

    // Queries work over the socket too, and see the transport.
    send(
        &mut ws,
        json!({ "id": "2", "type": "subscribe", "payload": { "query": "{ transport }" } }),
    )
    .await;
    let (payloads, _) = collect(&mut ws, "2").await;
    assert_eq!(payloads[0]["data"]["transport"], "websocket");
}

#[tokio::test]
async fn the_legacy_graphql_ws_protocol_is_supported() {
    let addr = serve(plugin()).await;
    let mut ws = connect(addr, Some("graphql-ws"), None).await.unwrap();
    init(&mut ws, json!({})).await;
    send(
        &mut ws,
        json!({ "id": "1", "type": "start", "payload": { "query": "subscription { ticks(count: 2) }" } }),
    )
    .await;
    let (payloads, end) = collect(&mut ws, "1").await;
    assert_eq!(end["type"], "complete");
    assert_eq!(payloads.len(), 2);
}

#[tokio::test]
async fn subscriptions_pass_the_same_pipeline_as_http() {
    let addr = serve(
        plugin()
            .configure(|c| c.limits.max_depth = 2)
            .mask_unexpected_errors(true),
    )
    .await;
    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    init(&mut ws, json!({})).await;
    send(
        &mut ws,
        json!({ "id": "deep", "type": "subscribe", "payload": {
            "query": "subscription { nested { nested { nested { value } } } }"
        } }),
    )
    .await;
    let (payloads, end) = collect(&mut ws, "deep").await;
    let text = format!("{payloads:?} {end}");
    assert!(text.contains("QUERY_LIMIT_EXCEEDED"), "{text}");
}

#[tokio::test]
async fn upgrade_headers_and_init_payload_reach_the_context() {
    let addr = serve(with_viewer_hooks(plugin())).await;

    let mut ws = connect(addr, Some("graphql-transport-ws"), Some("alice"))
        .await
        .unwrap();
    init(&mut ws, json!({})).await;
    send(&mut ws, json!({ "id": "1", "type": "subscribe", "payload": { "query": "subscription { viewer }" } })).await;
    let (payloads, _) = collect(&mut ws, "1").await;
    assert_eq!(payloads[0]["data"]["viewer"], "alice");

    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    init(&mut ws, json!({ "token": "t0k" })).await;
    send(&mut ws, json!({ "id": "1", "type": "subscribe", "payload": { "query": "subscription { viewer }" } })).await;
    let (payloads, _) = collect(&mut ws, "1").await;
    assert_eq!(payloads[0]["data"]["viewer"], "token:t0k");

    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    send(
        &mut ws,
        json!({ "type": "connection_init", "payload": { "token": "bad" } }),
    )
    .await;
    let message = recv(&mut ws).await;
    assert_eq!(message["close"], 4403, "{message}");
}

#[tokio::test]
async fn clients_that_never_initialise_are_closed_with_4408() {
    let addr = serve(plugin().configure(|c| c.subscriptions.init_timeout_secs = 1)).await;
    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    let message = recv(&mut ws).await;
    assert_eq!(message["close"], CLOSE_INIT_TIMEOUT, "{message}");
}

#[tokio::test]
async fn the_connection_cap_refuses_extra_sockets() {
    let addr = serve(plugin().configure(|c| c.subscriptions.max_connections = 1)).await;
    let mut first = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    init(&mut first, json!({})).await;
    match connect(addr, Some("graphql-transport-ws"), None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 503),
        other => panic!("expected a 503, got {other:?}"),
    }
    drop(first);
    // The slot is released when the first socket goes away.
    let mut retried = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(ws) = connect(addr, Some("graphql-transport-ws"), None).await {
            retried = Some(ws);
            break;
        }
    }
    assert!(retried.is_some(), "slot released");
}

#[tokio::test]
async fn an_unknown_subprotocol_is_refused() {
    let addr = serve(plugin()).await;
    match connect(addr, None, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 400),
        other => panic!("expected a 400, got {other:?}"),
    }
}

#[tokio::test]
async fn draining_closes_sockets_with_going_away() {
    let plugin = plugin();
    let drain = plugin.drain_handle();
    let addr = serve(plugin).await;
    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    init(&mut ws, json!({})).await;
    send(&mut ws, json!({ "id": "1", "type": "subscribe", "payload": { "query": "subscription { forever }" } })).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    drain.drain();
    let message = recv(&mut ws).await;
    assert_eq!(message["close"], CLOSE_GOING_AWAY, "{message}");
    assert_eq!(u16::from(CloseCode::Away), CLOSE_GOING_AWAY);

    // New connections are still accepted after a drain.
    let mut ws = connect(addr, Some("graphql-transport-ws"), None)
        .await
        .unwrap();
    init(&mut ws, json!({})).await;
}

// ── SSE ─────────────────────────────────────────────────────────────────────

fn sse_events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_owned());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            event.map(|e| (e, data))
        })
        .collect()
}

#[tokio::test]
async fn sse_streams_subscriptions_and_single_results() {
    let client = common::client(plugin());
    let response = client
        .post("/graphql")
        .header("accept", "text/event-stream")
        .json(&json!({ "query": "subscription { ticks(count: 2) }" }))
        .send()
        .await;
    assert_eq!(response.status, 200);
    assert!(
        common::header(&response, "content-type")
            .unwrap()
            .starts_with("text/event-stream")
    );
    let events = sse_events(&response.text());
    let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(names, ["next", "next", "complete"], "{events:?}");
    let first: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(first["data"]["ticks"], 1);

    // A query over SSE: one `next`, then `complete`. GET works too.
    let response = client
        .get("/graphql?query=%7B%20transport%20%7D")
        .header("accept", "text/event-stream")
        .send()
        .await;
    let events = sse_events(&response.text());
    assert_eq!(events.len(), 2, "{events:?}");
    let first: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(first["data"]["transport"], "sse");
}

#[tokio::test]
async fn sse_refusals_are_graphql_errors() {
    let client = common::client(plugin().configure(|c| c.limits.max_depth = 2));
    let response = client
        .post("/graphql")
        .header("accept", "text/event-stream")
        .json(&json!({ "query": "subscription { nested { nested { nested { value } } } }" }))
        .send()
        .await;
    let events = sse_events(&response.text());
    let first: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(
        first["errors"][0]["extensions"]["code"],
        "QUERY_LIMIT_EXCEEDED"
    );
    assert_eq!(events.last().unwrap().0, "complete");
}

#[tokio::test]
async fn sse_serves_trusted_documents_and_honours_hook_refusals() {
    use autumn_plugin_graphql::{PersistedQueryMode, TrustedDocuments};
    let mut documents = TrustedDocuments::new();
    documents.insert("ticks-v1", "subscription { ticks(count: 1) }");
    let client = common::client(
        plugin()
            .persisted_queries(PersistedQueryMode::Trusted)
            .trusted_documents(documents)
            .context(|parts, _state, _data| {
                Box::pin(async move {
                    if parts.headers.contains_key("x-deny") {
                        return Err(autumn_web::AutumnError::forbidden_msg("denied"));
                    }
                    Ok(())
                })
            }),
    );
    let sse = |body: Value, deny: bool| {
        let client = &client;
        async move {
            let mut request = client
                .post("/graphql")
                .header("accept", "text/event-stream");
            if deny {
                request = request.header("x-deny", "1");
            }
            request.json(&body).send().await
        }
    };

    let events = sse_events(&sse(json!({ "documentId": "ticks-v1" }), false).await.text());
    let first: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(first["data"]["ticks"], 1, "{events:?}");

    let refused = sse(json!({ "documentId": "unknown" }), false).await;
    assert_eq!(refused.status, 404);
    assert_eq!(
        refused.json::<Value>()["errors"][0]["extensions"]["code"],
        "PERSISTED_QUERY_NOT_FOUND"
    );

    let denied = sse(json!({ "documentId": "ticks-v1" }), true).await;
    assert_eq!(denied.status, 403);
    assert_eq!(
        denied.json::<Value>()["errors"][0]["extensions"]["code"],
        "FORBIDDEN"
    );
}

/// Regression: `GET` + `Accept: text/event-stream` must not become a way to
/// run mutations over `GET`. Subscriptions stay allowed (`EventSource` can
/// only `GET`).
#[tokio::test]
async fn sse_over_get_refuses_mutations_but_allows_subscriptions() {
    let client = common::client(plugin());
    let mutation = "mutation%20%7B%20createNote(title%3A%20%22x%22)%20%7B%20id%20%7D%20%7D";
    let response = client
        .get(&format!("/graphql?query={mutation}"))
        .header("accept", "text/event-stream")
        .send()
        .await;
    assert_eq!(response.status, 405);
    let (_, body) = common::post(&client, "{ notes { id } }", json!({})).await;
    assert_eq!(
        body["data"]["notes"].as_array().unwrap().len(),
        2,
        "nothing executed"
    );

    let subscription = "subscription%20%7B%20ticks(count%3A%201)%20%7D";
    let response = client
        .get(&format!("/graphql?query={subscription}"))
        .header("accept", "text/event-stream")
        .send()
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(sse_events(&response.text()).len(), 2);
}
