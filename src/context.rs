//! What every resolver can read from its [`async_graphql::Context`].
//!
//! The plugin inserts, into every operation's context data:
//!
//! - the request's [`AppState`] — so `ctx.data::<AppState>()?.pool()`,
//!   `.extension::<T>()` and everything else a route handler can reach is
//!   reachable from a resolver;
//! - a [`GraphqlRequestInfo`] describing the transport request (method, path,
//!   headers, request id, transport kind);
//! - whatever the app's [`context hooks`](crate::GraphqlPlugin::context) add,
//!   typically the authenticated principal and per-request `DataLoader`s.
//!
//! Hooks receive the request's [`Parts`], so any Autumn/axum extractor that
//! implements `FromRequestParts<AppState>` — sessions, the current user,
//! tenant resolution — works unchanged:
//!
//! ```ignore
//! GraphqlPlugin::new(schema).context(|parts, state, data| {
//!     Box::pin(async move {
//!         let user = CurrentUser::from_request_parts(parts, state).await?;
//!         data.insert(user);
//!         Ok(())
//!     })
//! })
//! ```

use std::sync::Arc;

use async_graphql::Data;
use autumn_web::{AppState, AutumnResult};
use http::request::Parts;
use http::{HeaderMap, Method, Uri};

use crate::persisted::BoxFuture;

/// Which transport carried an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Transport {
    /// `POST {path}` (JSON, `application/graphql`, or multipart).
    HttpPost,
    /// `GET {path}?query=…` — queries only.
    HttpGet,
    /// `GET {path}/ws` upgraded to a WebSocket.
    WebSocket,
    /// A `text/event-stream` response (GraphQL over SSE).
    Sse,
}

impl Transport {
    /// Label used in logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HttpPost => "http_post",
            Self::HttpGet => "http_get",
            Self::WebSocket => "websocket",
            Self::Sse => "sse",
        }
    }
}

/// The transport request an operation arrived on. Available to every
/// resolver as `ctx.data::<GraphqlRequestInfo>()`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GraphqlRequestInfo {
    /// HTTP method of the request (for a WebSocket, of the upgrade request).
    pub method: Method,
    /// Request URI.
    pub uri: Uri,
    /// Request headers.
    pub headers: HeaderMap,
    /// Autumn's request id, when the request-id middleware assigned one.
    pub request_id: Option<String>,
    /// Which transport carried the operation.
    pub transport: Transport,
    /// The mount path of the endpoint that served it.
    pub endpoint: Arc<str>,
}

impl GraphqlRequestInfo {
    pub(crate) fn from_parts(parts: &Parts, transport: Transport, endpoint: &Arc<str>) -> Self {
        let request_id = parts
            .extensions
            .get::<autumn_web::middleware::RequestId>()
            .map(ToString::to_string)
            .or_else(|| {
                parts
                    .headers
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            });
        Self {
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
            request_id,
            transport,
            endpoint: Arc::clone(endpoint),
        }
    }
}

/// A per-request context hook.
///
/// It adds data to the operation's context from the request's parts and
/// the app state. Returning `Err` refuses the request
/// with that error's HTTP status (for example `401`) before anything
/// executes.
pub trait ContextHook: Send + Sync + 'static {
    /// Populate `data` for one request.
    fn extend<'a>(
        &'a self,
        parts: &'a mut Parts,
        state: &'a AppState,
        data: &'a mut Data,
    ) -> BoxFuture<'a, AutumnResult<()>>;
}

impl<F> ContextHook for F
where
    F: for<'a> Fn(&'a mut Parts, &'a AppState, &'a mut Data) -> BoxFuture<'a, AutumnResult<()>>
        + Send
        + Sync
        + 'static,
{
    fn extend<'a>(
        &'a self,
        parts: &'a mut Parts,
        state: &'a AppState,
        data: &'a mut Data,
    ) -> BoxFuture<'a, AutumnResult<()>> {
        self(parts, state, data)
    }
}

/// Handles a WebSocket client's `connection_init` payload.
///
/// Browser clients put credentials there, since they cannot set headers on the
/// upgrade). The returned data joins the connection's context; returning
/// `Err` closes the connection with `4403 Forbidden`.
pub trait WsInitHook: Send + Sync + 'static {
    /// Validate the payload and produce connection data.
    fn on_init(
        &self,
        payload: serde_json::Value,
        state: AppState,
    ) -> BoxFuture<'static, AutumnResult<Data>>;
}

impl<F, Fut> WsInitHook for F
where
    F: Fn(serde_json::Value, AppState) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = AutumnResult<Data>> + Send + 'static,
{
    fn on_init(
        &self,
        payload: serde_json::Value,
        state: AppState,
    ) -> BoxFuture<'static, AutumnResult<Data>> {
        Box::pin(self(payload, state))
    }
}

/// Run every hook in order against one request.
pub(crate) async fn run_hooks(
    hooks: &[Arc<dyn ContextHook>],
    parts: &mut Parts,
    state: &AppState,
    data: &mut Data,
) -> AutumnResult<()> {
    for hook in hooks {
        hook.extend(parts, state, data).await?;
    }
    Ok(())
}
