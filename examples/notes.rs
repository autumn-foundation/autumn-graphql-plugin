//! A complete GraphQL API on Autumn in one file.
//!
//! ```text
//! cargo run --example notes
//!
//! # query
//! curl -s localhost:3000/graphql -H 'content-type: application/json' \
//!   -d '{"query":"{ notes { id title author } }"}'
//!
//! # mutation (the context hook turns the bearer token into a Viewer)
//! curl -s localhost:3000/graphql -H 'content-type: application/json' \
//!   -H 'authorization: Bearer ada' \
//!   -d '{"query":"mutation { addNote(title: \"hello\") { id author } }"}'
//!
//! # subscription over SSE (or connect a graphql-ws client to /graphql/ws)
//! curl -N localhost:3000/graphql -H 'accept: text/event-stream' \
//!   -H 'content-type: application/json' \
//!   -d '{"query":"subscription { noteAdded { title author } }"}'
//! ```
//!
//! The pieces:
//!
//! - the store is an `AppState` extension (installed with
//!   `state_initializer`), reached from resolvers through
//!   `ctx.data::<AppState>()` — the same way a route handler reaches it;
//! - a context hook authenticates every request and every WebSocket upgrade;
//! - errors meant for clients carry an `extensions.code`, so they survive
//!   production masking;
//! - subscriptions fan out through a `tokio::sync::broadcast` channel.

#![allow(clippy::unused_async)] // async-graphql resolvers are `async fn`

use std::sync::{Arc, RwLock};

use async_graphql::{Context, Object, Result, Schema, SimpleObject, Subscription};
use autumn_plugin_graphql::error::{codes, graphql_error};
use autumn_plugin_graphql::{GraphqlPlugin, PersistedQueryMode};
use autumn_web::AppState;
use futures_util::Stream;
use tokio::sync::broadcast;

#[derive(Clone, SimpleObject)]
struct Note {
    id: u32,
    title: String,
    author: String,
}

/// The app's store, published into `AppState`.
struct NoteStore {
    notes: RwLock<Vec<Note>>,
    added: broadcast::Sender<Note>,
}

impl NoteStore {
    fn new() -> Self {
        Self {
            notes: RwLock::new(vec![Note {
                id: 1,
                title: "Welcome to GraphQL on Autumn".into(),
                author: "system".into(),
            }]),
            added: broadcast::channel(64).0,
        }
    }
}

/// Who is calling, as the context hook established it.
struct Viewer(String);

fn store(ctx: &Context<'_>) -> Result<Arc<NoteStore>> {
    ctx.data::<AppState>()?
        .extension::<NoteStore>()
        .ok_or_else(|| graphql_error(codes::INTERNAL_SERVER_ERROR, "store missing", Some(500)))
}

struct Query;

#[Object]
impl Query {
    /// Every note, oldest first.
    async fn notes(&self, ctx: &Context<'_>) -> Result<Vec<Note>> {
        let store = store(ctx)?;
        let notes = store.notes.read().map_err(|_| "store poisoned")?;
        Ok(notes.clone())
    }
}

struct Mutation;

#[Object]
impl Mutation {
    /// Add a note. Requires a viewer.
    async fn add_note(&self, ctx: &Context<'_>, title: String) -> Result<Note> {
        let Some(Viewer(author)) = ctx.data_opt::<Viewer>() else {
            return Err(graphql_error(
                codes::UNAUTHENTICATED,
                "sign in to add notes",
                Some(401),
            ));
        };
        let title = title.trim().to_owned();
        if title.is_empty() || title.len() > 120 {
            return Err(graphql_error(
                codes::VALIDATION_FAILED,
                "title must be 1–120 characters",
                Some(422),
            ));
        }
        let store = store(ctx)?;
        let note = {
            let mut notes = store.notes.write().map_err(|_| "store poisoned")?;
            let note = Note {
                id: u32::try_from(notes.len())
                    .unwrap_or(u32::MAX)
                    .saturating_add(1),
                title,
                author: author.clone(),
            };
            notes.push(note.clone());
            note
        };
        // No subscribers is fine.
        let _ = store.added.send(note.clone());
        Ok(note)
    }
}

struct Subscriptions;

#[Subscription]
impl Subscriptions {
    /// Every note added from now on.
    async fn note_added(&self, ctx: &Context<'_>) -> Result<impl Stream<Item = Note>> {
        let receiver = store(ctx)?.added.subscribe();
        Ok(futures_util::stream::unfold(
            receiver,
            |mut receiver| async move {
                loop {
                    match receiver.recv().await {
                        Ok(note) => return Some((note, receiver)),
                        // A slow subscriber skips what it missed rather than dying.
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            },
        ))
    }
}

/// Autumn refuses to boot an app with no typed routes, and routers a plugin
/// nests do not count — so a GraphQL-only service registers at least one.
#[autumn_web::get("/")]
async fn index() -> &'static str {
    "GraphQL API: POST /graphql, GET /graphql/sdl, WebSocket /graphql/ws\n"
}

#[autumn_web::main]
async fn main() {
    let schema = Schema::build(Query, Mutation, Subscriptions).finish();

    autumn_web::app()
        .routes(autumn_web::routes![index])
        // `state_initializer` puts the store into `AppState` (builder
        // `with_extension` values stay on the builder, for plugins).
        .state_initializer(|state| state.insert_extension(NoteStore::new()))
        .plugin(
            GraphqlPlugin::new(schema)
                // Demo auth: `Authorization: Bearer <name>` becomes the viewer.
                // A real app runs its session/token extractor here instead.
                .context(|parts, _state, data| {
                    Box::pin(async move {
                        let viewer = parts
                            .headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("Bearer "))
                            .map(str::to_owned);
                        if let Some(name) = viewer {
                            data.insert(Viewer(name));
                        }
                        Ok(())
                    })
                })
                // Browser WebSocket clients send credentials in connection_init.
                .on_ws_init(|payload: serde_json::Value, _state| async move {
                    let mut data = async_graphql::Data::default();
                    if let Some(name) = payload["token"].as_str() {
                        data.insert(Viewer(name.to_owned()));
                    }
                    Ok(data)
                })
                .persisted_queries(PersistedQueryMode::Automatic)
                .configure(|c| c.limits.max_depth = 8),
        )
        .run()
        .await;
}
