//! [`GraphqlPlugin`]: mount any async-graphql executor on an Autumn app.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_graphql::{Executor, ObjectType, Schema, SchemaBuilder, SubscriptionType};
use autumn_web::AppState;
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::route_listing::{RouteClassification, RouteInfo};
use axum::Router;
use axum::extract::Extension;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use tokio::sync::{Semaphore, watch};

use crate::config::{
    ConfigError, DEFAULT_SECTION, GraphqlConfig, PersistedQueryMode, Resolved, Toggle,
};
use crate::context::{ContextHook, WsInitHook};
use crate::metrics::Metrics;
use crate::persisted::{InMemoryPersistedQueryStore, PersistedQueryStore, TrustedDocuments};
use crate::pipeline::{Pipeline, PipelineSettings};
use crate::transport::{Endpoint, http, ws};

/// Name this plugin reports in its contract, in `autumn routes`, and in
/// `autumn plugin-check` output.
pub const PLUGIN_NAME: &str = "autumn-plugin-graphql";

/// The `autumn-web` series this release is verified against, as the semver
/// requirement declared in [`Plugin::contract`]. Autumn's startup gate and
/// `autumn plugin-check` refuse any other series.
pub const SUPPORTED_AUTUMN_WEB: &str = "0.8";

type Materialize<E> = Box<dyn FnOnce(&GraphqlConfig) -> (E, Option<String>) + Send>;
type RouterTransform = Box<dyn FnOnce(Router<AppState>) -> Router<AppState> + Send>;
type Override = Arc<dyn Fn(&mut GraphqlConfig) + Send + Sync>;

enum Source<E> {
    Ready { executor: E, sdl: Option<String> },
    Deferred(Materialize<E>),
}

enum ConfigSource {
    /// Read `[section]` from the app's files and environment.
    Resolve(String),
    /// Use this configuration; read no files.
    Explicit(Box<GraphqlConfig>),
}

struct Guard {
    apply: RouterTransform,
    label: String,
}

/// Mounts an async-graphql schema (or any [`Executor`]) on an Autumn app.
///
/// ```rust,ignore
/// use autumn_plugin_graphql::GraphqlPlugin;
///
/// autumn_web::app()
///     .plugin(GraphqlPlugin::new(schema))   // POST/GET /graphql, /graphql/sdl, /graphql/ws
///     .run()
///     .await;
/// ```
///
/// Configuration comes from the `[graphql]` section of `autumn.toml` (with
/// profile layering and `AUTUMN_GRAPHQL__*` overrides — see
/// [`GraphqlConfig`]); the fluent setters here are applied **on top** of it,
/// so code can pin a value that operations must not change. Call
/// [`config`](Self::config) to supply the whole configuration in code and
/// read no files.
pub struct GraphqlPlugin<E> {
    /// `None` only after `build` consumed it.
    source: Option<Source<E>>,
    subscriptions_supported: bool,
    has_sdl: bool,
    config_source: ConfigSource,
    overrides: Vec<Override>,
    resolved: OnceLock<Result<Resolved, ConfigError>>,
    development: Option<bool>,
    guard: Option<Guard>,
    hooks: Vec<Arc<dyn ContextHook>>,
    ws_init: Option<Arc<dyn WsInitHook>>,
    store: Option<Arc<dyn PersistedQueryStore>>,
    trusted: TrustedDocuments,
    drain: DrainHandle,
}

/// Closes every WebSocket and SSE stream an endpoint holds.
///
/// The plugin drains automatically when the app shuts down (sockets close
/// with `1001 Going Away`, SSE streams end with `complete`), so clients
/// reconnect to a healthy instance. Take a handle with
/// [`GraphqlPlugin::drain_handle`] to drain earlier — for example when a
/// readiness probe flips during a rolling deploy. Draining is permanent for
/// the streams open at the time; new connections are still accepted.
#[derive(Clone, Debug)]
pub struct DrainHandle {
    /// A generation counter: every drain bumps it, and a stream ends on the
    /// first change after it subscribed. (A boolean set-then-reset would let
    /// a slow receiver see only the reset and miss the drain.)
    tx: watch::Sender<u64>,
}

impl DrainHandle {
    pub(crate) fn new() -> Self {
        Self {
            tx: watch::channel(0).0,
        }
    }

    /// Close every stream open at this moment.
    pub fn drain(&self) {
        self.tx
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// A receiver whose current generation is already marked seen.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }
}

impl<Q, M, S> GraphqlPlugin<Schema<Q, M, S>>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    /// Mount a built schema. Static limits are enforced by the plugin's own
    /// analysis; for async-graphql's complexity model as well, use
    /// [`from_builder`](Self::from_builder).
    #[must_use]
    pub fn new(schema: Schema<Q, M, S>) -> Self {
        let sdl = schema.sdl();
        Self::with_source(
            Source::Ready {
                executor: schema,
                sdl: Some(sdl),
            },
            !S::is_empty(),
            true,
        )
    }

    /// Mount a schema the plugin finishes itself, so the configured
    /// `limits.max_depth`, `limits.max_complexity` and
    /// `limits.max_directives` are also installed as async-graphql's own
    /// schema-level limits (defence in depth, and the only way to use its
    /// `#[graphql(complexity = …)]` cost model).
    #[must_use]
    pub fn from_builder(builder: SchemaBuilder<Q, M, S>) -> Self
    where
        SchemaBuilder<Q, M, S>: Send,
    {
        Self::with_source(
            Source::Deferred(Box::new(move |config: &GraphqlConfig| {
                let limits = &config.limits;
                let mut builder = builder;
                if limits.max_depth != 0 {
                    builder = builder.limit_depth(limits.max_depth);
                }
                if limits.max_complexity != 0 {
                    builder = builder.limit_complexity(limits.max_complexity);
                }
                if limits.max_directives != 0 {
                    builder = builder.limit_directives(limits.max_directives);
                }
                let schema = builder.finish();
                let sdl = schema.sdl();
                (schema, Some(sdl))
            })),
            !S::is_empty(),
            true,
        )
    }
}

impl<E: Executor> GraphqlPlugin<E> {
    /// Mount any [`Executor`] — for example an
    /// `async_graphql::dynamic::Schema` built at runtime. Subscriptions are
    /// assumed supported (turn the transports off in `[graphql.subscriptions]`
    /// if they are not); provide the SDL with [`sdl_text`](Self::sdl_text) to
    /// serve `GET {path}/sdl`.
    #[must_use]
    pub fn from_executor(executor: E) -> Self {
        Self::with_source(
            Source::Ready {
                executor,
                sdl: None,
            },
            true,
            false,
        )
    }

    fn with_source(source: Source<E>, subscriptions_supported: bool, has_sdl: bool) -> Self {
        Self {
            source: Some(source),
            subscriptions_supported,
            has_sdl,
            config_source: ConfigSource::Resolve(DEFAULT_SECTION.to_owned()),
            overrides: Vec::new(),
            resolved: OnceLock::new(),
            development: None,
            guard: None,
            hooks: Vec::new(),
            ws_init: None,
            store: None,
            trusted: TrustedDocuments::new(),
            drain: DrainHandle::new(),
        }
    }

    /// A handle that closes every open WebSocket and SSE stream of this
    /// endpoint on demand (see [`DrainHandle`]).
    #[must_use]
    pub fn drain_handle(&self) -> DrainHandle {
        self.drain.clone()
    }

    /// Supply the SDL served at `GET {path}/sdl` (for
    /// [`from_executor`](Self::from_executor) mounts).
    #[must_use]
    pub fn sdl_text(mut self, sdl: impl Into<String>) -> Self {
        let sdl = sdl.into();
        self.has_sdl = true;
        self.source = match self.source {
            Some(Source::Ready { executor, .. }) => Some(Source::Ready {
                executor,
                sdl: Some(sdl),
            }),
            other => other,
        };
        self
    }

    fn reset(&mut self) {
        self.resolved = OnceLock::new();
    }

    /// Adjust the configuration in code, on top of whatever the files say.
    ///
    /// ```rust,ignore
    /// GraphqlPlugin::new(schema).configure(|c| {
    ///     c.limits.max_depth = 8;
    ///     c.batching.enabled = true;
    /// })
    /// ```
    #[must_use]
    pub fn configure(mut self, apply: impl Fn(&mut GraphqlConfig) + Send + Sync + 'static) -> Self {
        self.overrides.push(Arc::new(apply));
        self.reset();
        self
    }

    /// Use `config` as the whole configuration: no `autumn.toml` section and
    /// no environment overrides are read. Fluent setters still apply on top.
    #[must_use]
    pub fn config(mut self, config: GraphqlConfig) -> Self {
        self.config_source = ConfigSource::Explicit(Box::new(config));
        self.reset();
        self
    }

    /// Read a different top-level section than `[graphql]` — needed when an
    /// app mounts two endpoints with different settings.
    #[must_use]
    pub fn config_section(mut self, section: impl Into<String>) -> Self {
        self.config_source = ConfigSource::Resolve(section.into());
        self.reset();
        self
    }

    /// Force development (`true`) or production (`false`) defaults for the
    /// `auto` settings, instead of deriving them from the active profile.
    #[must_use]
    pub fn development(mut self, development: bool) -> Self {
        self.development = Some(development);
        self.reset();
        self
    }

    /// Mount under a different path (for example `/api/graphql`).
    #[must_use]
    pub fn path(self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.configure(move |c| c.path.clone_from(&path))
    }

    /// Allow or forbid introspection regardless of profile.
    #[must_use]
    pub fn introspection(self, enabled: bool) -> Self {
        self.configure(move |c| c.introspection = enabled.into())
    }

    /// Do not serve `GET {path}/sdl`.
    #[must_use]
    pub fn without_sdl(self) -> Self {
        self.configure(|c| c.sdl = Toggle::Off)
    }

    /// Mask (or stop masking) uncoded resolver errors regardless of profile.
    #[must_use]
    pub fn mask_unexpected_errors(self, mask: bool) -> Self {
        self.configure(move |c| c.mask_unexpected_errors = mask.into())
    }

    /// Per-operation execution timeout (`Duration::ZERO` = none).
    #[must_use]
    pub fn timeout(self, timeout: Duration) -> Self {
        let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.configure(move |c| c.timeout_ms = millis)
    }

    /// Accept batched requests of up to `max_operations` operations.
    #[must_use]
    pub fn batching(self, max_operations: usize) -> Self {
        self.configure(move |c| {
            c.batching.enabled = true;
            c.batching.max_operations = max_operations;
        })
    }

    /// Set the persisted-query mode.
    #[must_use]
    pub fn persisted_queries(self, mode: PersistedQueryMode) -> Self {
        self.configure(move |c| c.persisted_queries.mode = mode)
    }

    /// Add trusted documents in code (merged with any manifest file).
    #[must_use]
    pub fn trusted_documents(mut self, documents: TrustedDocuments) -> Self {
        self.trusted.extend(documents);
        self
    }

    /// Replace the in-memory APQ cache (e.g. with a Redis-backed store shared
    /// by every instance).
    #[must_use]
    pub fn persisted_query_store(mut self, store: impl PersistedQueryStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// Add a per-request [`ContextHook`]. Hooks run in registration order on
    /// every HTTP operation and on each WebSocket upgrade.
    ///
    /// ```rust,ignore
    /// GraphqlPlugin::new(schema).context(|parts, state, data| {
    ///     Box::pin(async move {
    ///         let user = CurrentUser::from_request_parts(parts, state).await?;
    ///         data.insert(user);
    ///         Ok(())
    ///     })
    /// })
    /// ```
    #[must_use]
    pub fn context<F>(mut self, hook: F) -> Self
    where
        F: for<'a> Fn(
                &'a mut ::http::request::Parts,
                &'a AppState,
                &'a mut async_graphql::Data,
            ) -> crate::persisted::BoxFuture<'a, autumn_web::AutumnResult<()>>
            + Send
            + Sync
            + 'static,
    {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Add a [`ContextHook`] implemented as a type.
    #[must_use]
    pub fn context_hook(mut self, hook: impl ContextHook) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Handle the WebSocket `connection_init` payload (see [`WsInitHook`]).
    #[must_use]
    pub fn on_ws_init(mut self, hook: impl WsInitHook) -> Self {
        self.ws_init = Some(Arc::new(hook));
        self
    }

    /// Guard every route this plugin mounts with `layer` — for example
    /// `RequireApiToken`, so the endpoint needs `Authorization: Bearer …`.
    ///
    /// This is the seam a nested router needs: `AppBuilder::scoped(prefix,
    /// layer, routes![…])` wraps only the routes handed to it, never a raw
    /// router a plugin nests. The layer is the outermost on the plugin's
    /// router, so it runs before any handler, including `/sdl` and the
    /// WebSocket upgrade. `label` is what `autumn routes` lists as the
    /// route's middleware; the routes are then classified `Gated`.
    #[must_use]
    pub fn guard<L>(mut self, layer: L, label: impl Into<String>) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<axum::extract::Request> + Clone + Send + Sync + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Response: IntoResponse + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Error:
            Into<std::convert::Infallible> + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
    {
        self.guard = Some(Guard {
            apply: Box::new(move |router| router.layer(layer)),
            label: label.into(),
        });
        self
    }

    /// The configuration this plugin will run with, after files,
    /// environment and code overrides.
    ///
    /// # Errors
    ///
    /// The [`ConfigError`] that will abort boot.
    pub fn effective_config(&self) -> Result<&GraphqlConfig, &ConfigError> {
        self.resolved().as_ref().map(|r| &r.config)
    }

    fn resolved(&self) -> &Result<Resolved, ConfigError> {
        self.resolved.get_or_init(|| {
            let mut resolved = match &self.config_source {
                ConfigSource::Resolve(section) => GraphqlConfig::resolve(section)?,
                ConfigSource::Explicit(config) => Resolved::explicit((**config).clone()),
            };
            for apply in &self.overrides {
                apply(&mut resolved.config);
            }
            if let Some(development) = self.development {
                if development { "dev" } else { "prod" }.clone_into(&mut resolved.profile);
            }
            resolved.config.validate()?;
            Ok(resolved)
        })
    }

    /// The effective configuration, or the defaults (with overrides) when
    /// resolution failed — the failure itself aborts boot from the startup
    /// hook; this keeps every other path total.
    fn config_or_default(&self) -> (GraphqlConfig, bool) {
        self.resolved().as_ref().map_or_else(
            |_| {
                let mut config = GraphqlConfig::default();
                for apply in &self.overrides {
                    apply(&mut config);
                }
                (config, self.development.unwrap_or(false))
            },
            |resolved| (resolved.config.clone(), resolved.is_development()),
        )
    }

    fn mount_plan(&self) -> MountPlan {
        let (config, development) = self.config_or_default();
        let introspection = config.introspection.resolve(development);
        MountPlan {
            sdl: self.has_sdl && config.sdl.resolve(introspection),
            websocket: config
                .subscriptions
                .websocket
                .resolve(self.subscriptions_supported),
            sse: config
                .subscriptions
                .sse
                .resolve(self.subscriptions_supported),
            introspection,
            development,
            config,
        }
    }

    /// The routes this plugin mounts, as `autumn routes` lists them.
    ///
    /// Without a [`guard`](Self::guard) every route is classified
    /// [`RouteClassification::Public`]; with one, `Gated` with the guard's
    /// label as middleware.
    #[must_use]
    pub fn route_infos(&self) -> Vec<RouteInfo> {
        let plan = self.mount_plan();
        if !plan.config.enabled {
            return Vec::new();
        }
        let path = plan.config.path.trim().to_owned();
        let guard = self.guard.as_ref().map(|g| g.label.as_str());
        let mut routes = vec![route("POST", path.clone(), "post_graphql", guard)];
        if plan.config.allow_get || plan.sse {
            routes.push(route("GET", path.clone(), "get_graphql", guard));
        }
        if plan.sdl {
            routes.push(route("GET", format!("{path}/sdl"), "sdl", guard));
        }
        if plan.websocket {
            routes.push(route("GET", format!("{path}/ws"), "websocket", guard));
        }
        routes
    }

    fn trusted_documents_for(&self, plan: &MountPlan) -> Result<TrustedDocuments, String> {
        let mut trusted = self.trusted.clone();
        let manifest = plan.config.persisted_queries.manifest.trim();
        if !manifest.is_empty() {
            let base = self
                .resolved()
                .as_ref()
                .map_or_else(|_| std::path::PathBuf::from("."), |r| r.base_dir.clone());
            let path = base.join(manifest);
            trusted.extend(TrustedDocuments::from_file(&path).map_err(|e| e.to_string())?);
        }
        if plan.config.persisted_queries.mode == PersistedQueryMode::Trusted && trusted.is_empty() {
            return Err(
                "graphql.persisted_queries.mode = \"trusted\" needs a manifest (or documents \
                 added with `trusted_documents`); with none, every operation would be refused"
                    .to_owned(),
            );
        }
        Ok(trusted)
    }

    /// Assemble the request-time state of one endpoint.
    fn endpoint(
        &mut self,
        plan: &MountPlan,
        path: &str,
        executor: E,
        sdl: Option<String>,
        trusted: TrustedDocuments,
    ) -> Endpoint<E> {
        let config = &plan.config;
        let store = self.store.take().unwrap_or_else(|| {
            Arc::new(InMemoryPersistedQueryStore::new(
                config.persisted_queries.cache_capacity,
            ))
        });
        let metrics = Metrics::new(path);
        let millis = |ms: u64| (ms > 0).then(|| Duration::from_millis(ms));
        let pipeline = Arc::new(Pipeline {
            settings: PipelineSettings {
                introspection: plan.introspection,
                mask_unexpected_errors: config.mask_unexpected_errors.resolve(!plan.development),
                timeout: millis(config.timeout_ms),
                slow_operation: millis(config.slow_operation_ms),
                limits: config.limits.clone(),
                persisted_mode: config.persisted_queries.mode,
            },
            store,
            trusted: Arc::new(trusted),
            metrics: metrics.clone(),
        });
        let subs = &config.subscriptions;
        Endpoint {
            executor,
            pipeline,
            hooks: std::mem::take(&mut self.hooks),
            ws_init: self.ws_init.take(),
            path: Arc::from(path),
            sdl: if plan.sdl { sdl.map(Arc::from) } else { None },
            allow_get: config.allow_get,
            max_body_bytes: config.max_body_bytes,
            batching: config.batching.clone(),
            uploads: config.uploads.clone(),
            csrf_prevention: config.csrf_prevention,
            sse: plan.sse,
            keepalive: Duration::from_secs(subs.keepalive_secs.max(1)),
            ws_init_timeout: Duration::from_secs(subs.init_timeout_secs.max(1)),
            streams: (subs.max_connections > 0)
                .then(|| Arc::new(Semaphore::new(subs.max_connections))),
            shutdown: self.drain.clone(),
            metrics,
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // a plan is a set of independent switches
struct MountPlan {
    config: GraphqlConfig,
    development: bool,
    introspection: bool,
    sdl: bool,
    websocket: bool,
    sse: bool,
}

fn route(method: &str, path: String, handler: &str, guard: Option<&str>) -> RouteInfo {
    RouteInfo {
        method: method.to_owned(),
        path,
        handler: format!("autumn_plugin_graphql::{handler}"),
        classification: guard.map_or(RouteClassification::Public, |_| RouteClassification::Gated),
        middleware: guard.map(str::to_owned).into_iter().collect(),
        ..Default::default()
    }
}

impl<E: Executor> Plugin for GraphqlPlugin<E> {
    /// Keyed by mount path, so two endpoints can coexist at different paths
    /// while a second plugin at the *same* path is refused as a duplicate.
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!(
            "{PLUGIN_NAME}@{}",
            self.mount_plan().config.path.trim()
        ))
    }

    /// Declares the supported `autumn-web` series ([`SUPPORTED_AUTUMN_WEB`]).
    fn contract(&self) -> Option<PluginContract> {
        Some(
            PluginContract::new(PLUGIN_NAME)
                .plugin_version(env!("CARGO_PKG_VERSION"))
                .autumn_web(SUPPORTED_AUTUMN_WEB),
        )
    }

    fn build(mut self, app: AppBuilder) -> AppBuilder {
        let app = match &self.config_source {
            ConfigSource::Resolve(section) => app.config_section(section.clone()),
            ConfigSource::Explicit(_) => app,
        };
        let plan = self.mount_plan();
        let routes = self.route_infos();

        let mut problems: Vec<String> = Vec::new();
        if let Err(error) = self.resolved() {
            problems.push(error.to_string());
        }
        let trusted = self.trusted_documents_for(&plan).unwrap_or_else(|error| {
            problems.push(error);
            TrustedDocuments::new()
        });
        let app = app.on_startup(move |_state| {
            let problems = problems.clone();
            async move {
                if problems.is_empty() {
                    Ok(())
                } else {
                    Err(autumn_web::AutumnError::internal_server_error_msg(format!(
                        "{PLUGIN_NAME}: {}",
                        problems.join("; ")
                    )))
                }
            }
        });

        let config = plan.config.clone();
        if !config.enabled {
            tracing::info!(path = %config.path, "GraphQL endpoint disabled by configuration");
            return app;
        }
        let path = config.path.trim().to_owned();

        let (executor, sdl) = match self.source.take() {
            Some(Source::Ready { executor, sdl }) => (executor, sdl),
            Some(Source::Deferred(materialize)) => materialize(&config),
            None => return app,
        };

        let endpoint = Arc::new(self.endpoint(&plan, &path, executor, sdl, trusted));

        let mut endpoint_router = Router::new().route(
            "/",
            post(http::post_graphql::<E>).get(http::get_graphql::<E>),
        );
        if endpoint.sdl.is_some() {
            endpoint_router = endpoint_router.route("/sdl", get(http::sdl::<E>));
        }
        if plan.websocket {
            endpoint_router = endpoint_router.route("/ws", get(ws::upgrade::<E>));
        }
        let layered = endpoint_router.layer(Extension(endpoint));
        // Applied last, so the guard is the outermost layer and runs first.
        let guarded = match self.guard.take() {
            Some(guard) => (guard.apply)(layered),
            None => layered,
        };

        tracing::info!(
            path = %path,
            introspection = plan.introspection,
            sdl = plan.sdl,
            websocket = plan.websocket,
            sse = plan.sse,
            persisted_queries = ?config.persisted_queries.mode,
            "mounting GraphQL endpoint"
        );
        let drain = self.drain.clone();
        app.nest(&path, guarded)
            .declare_plugin_routes(routes)
            .on_shutdown(move || {
                let drain = drain.clone();
                async move { drain.drain() }
            })
    }
}
