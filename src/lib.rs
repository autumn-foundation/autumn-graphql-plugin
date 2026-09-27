//! Production-grade GraphQL for [Autumn](https://github.com/autumn-foundation/autumn).
//!
//! Mount any [async-graphql](https://docs.rs/async-graphql) schema on an
//! Autumn app with one line, and get the operational surface a public or
//! internal API needs:
//!
//! ```rust,ignore
//! use autumn_plugin_graphql::GraphqlPlugin;
//!
//! #[autumn_web::main]
//! async fn main() {
//!     autumn_web::app()
//!         .plugin(GraphqlPlugin::new(Schema::build(Query, Mutation, Subscription).finish()))
//!         .run()
//!         .await;
//! }
//! ```
//!
//! | Concern | What you get |
//! |---|---|
//! | Transports | `POST`/`GET` per GraphQL-over-HTTP (incl. `application/graphql-response+json` status semantics), opt-in batching, opt-in multipart uploads, WebSocket (`graphql-transport-ws` + legacy `graphql-ws`), SSE |
//! | Hardening | size, lexical-nesting, depth, alias, root-field, field-count and directive limits before execution; mutations refused over `GET`; introspection and SDL off outside dev; CSRF preflight for multipart; execution timeouts; connection caps |
//! | Persisted operations | Apollo APQ with a pluggable store, and trusted documents (safelisting) from an Apollo manifest |
//! | Errors | stable `extensions.code` on every error; `AutumnError` mapping with 5xx redaction; masking of uncoded resolver errors |
//! | Context | `AppState` + [`GraphqlRequestInfo`] in every resolver; hooks that run any axum extractor (current user, tenant, `DataLoaders`); WebSocket `connection_init` auth |
//! | Operations | `[graphql]` config with profile layering and `AUTUMN_GRAPHQL__*` overrides; kill switch; Prometheus metrics; tracing spans; slow-operation log; graceful WebSocket/SSE shutdown |
//! | Framework fit | declared routes for `autumn routes` / audit, a `PluginContract`, a guard seam for nested routers, conformance-tested |
//!
//! Every transport — HTTP, WebSocket and SSE — goes through the same
//! [`pipeline`], so no path reaches the schema without the same checks.
//!
//! See the crate README for a guided tour, and `docs/` for architecture
//! decision records.

pub mod config;
pub mod context;
pub mod error;
pub mod limits;
mod metrics;
pub mod persisted;
pub mod pipeline;
mod plugin;
pub mod sdl;
#[cfg(feature = "test-support")]
pub mod testing;
mod transport;

pub use config::{GraphqlConfig, PersistedQueryMode, Toggle};
pub use context::{ContextHook, GraphqlRequestInfo, Transport, WsInitHook};
pub use error::{GraphqlResultExt, IntoGraphqlError, codes};
pub use persisted::{InMemoryPersistedQueryStore, PersistedQueryStore, TrustedDocuments};
pub use plugin::{DrainHandle, GraphqlPlugin, PLUGIN_NAME, SUPPORTED_AUTUMN_WEB};
pub use transport::ws::{CLOSE_FORBIDDEN, CLOSE_GOING_AWAY, CLOSE_INIT_TIMEOUT};

/// Re-exported so apps and this crate always agree on one async-graphql.
pub use async_graphql;
