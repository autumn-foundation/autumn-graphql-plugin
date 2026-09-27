//! A small schema that exercises every plugin feature, shared by the
//! integration tests.

// async-graphql resolvers are `async fn` whether or not they await.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unused_async,
    missing_docs
)]

use std::sync::Mutex;
use std::time::Duration;

use async_graphql::{
    Context, EmptySubscription, ErrorExtensions, Object, Result, Schema, SimpleObject,
    Subscription, Upload,
};
use autumn_plugin_graphql::{GraphqlConfig, GraphqlPlugin, GraphqlRequestInfo, GraphqlResultExt};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::{AppState, AutumnError};
use futures_util::Stream;
use serde_json::{Value, json};

/// Who is calling, as a context hook would record it.
#[derive(Clone, Debug)]
pub struct Viewer(pub String);

/// Shared in-memory notes, handed to the schema as schema data.
#[derive(Default)]
pub struct Notes(pub Mutex<Vec<String>>);

#[derive(SimpleObject)]
pub struct Note {
    pub id: i32,
    pub title: String,
}

pub struct Query;

#[Object]
impl Query {
    async fn notes(&self, ctx: &Context<'_>) -> Result<Vec<Note>> {
        let notes = ctx.data::<Notes>()?;
        Ok(notes
            .0
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, title)| Note {
                id: i32::try_from(i).unwrap(),
                title: title.clone(),
            })
            .collect())
    }

    /// Resolvers see the request's `AppState`.
    async fn profile(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(ctx.data::<AppState>()?.profile().to_owned())
    }

    /// Resolvers see the transport request.
    async fn request_header(&self, ctx: &Context<'_>, name: String) -> Result<Option<String>> {
        let info = ctx.data::<GraphqlRequestInfo>()?;
        Ok(info
            .headers
            .get(name.as_str())
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned))
    }

    async fn transport(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(ctx
            .data::<GraphqlRequestInfo>()?
            .transport
            .as_str()
            .to_owned())
    }

    /// Set by a context hook.
    async fn viewer(&self, ctx: &Context<'_>) -> Option<String> {
        ctx.data_opt::<Viewer>().map(|v| v.0.clone())
    }

    async fn slow(&self, ms: u64) -> bool {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        true
    }

    /// An error with no code: "unexpected".
    async fn boom(&self) -> Result<i32> {
        Err("database password is hunter2".into())
    }

    /// An error with a code: meant for clients.
    async fn coded(&self) -> Result<i32> {
        Err(async_graphql::Error::new("slow down")
            .extend_with(|_, e| e.set("code", "RATE_LIMITED")))
    }

    async fn autumn_error(&self, status: u16) -> Result<i32> {
        let err = match status {
            404 => AutumnError::not_found_msg("note 9 not found"),
            422 => {
                let mut fields = std::collections::HashMap::new();
                fields.insert("title".to_owned(), vec!["must not be blank".to_owned()]);
                AutumnError::validation(fields)
            }
            _ => AutumnError::internal_server_error_msg("disk /dev/sda1 on fire"),
        };
        Err::<i32, _>(err).gql()
    }

    #[graphql(cache_control(max_age = 60))]
    async fn cached(&self) -> i32 {
        7
    }

    async fn sets_header(&self, ctx: &Context<'_>) -> bool {
        ctx.insert_http_header("x-from-resolver", "yes");
        true
    }

    async fn nested(&self) -> Nested {
        Nested
    }
}

pub struct Nested;

#[Object]
impl Nested {
    async fn value(&self) -> i32 {
        1
    }
    async fn nested(&self) -> Self {
        Self
    }
}

pub struct Mutation;

#[Object]
impl Mutation {
    async fn create_note(&self, ctx: &Context<'_>, title: String) -> Result<Note> {
        let notes = ctx.data::<Notes>()?;
        let mut notes = notes.0.lock().unwrap();
        notes.push(title.clone());
        Ok(Note {
            id: i32::try_from(notes.len() - 1).unwrap(),
            title,
        })
    }

    async fn upload(&self, ctx: &Context<'_>, file: Upload) -> Result<String> {
        let value = file.value(ctx)?;
        let size = value.size()?;
        Ok(format!("{}:{size}", value.filename))
    }
}

pub struct Subscriptions;

#[Subscription]
impl Subscriptions {
    async fn ticks(&self, count: u32) -> impl Stream<Item = u32> {
        futures_util::stream::iter(1..=count)
    }

    async fn viewer(&self, ctx: &Context<'_>) -> impl Stream<Item = Option<String>> {
        let viewer = ctx.data_opt::<Viewer>().map(|v| v.0.clone());
        futures_util::stream::iter([viewer])
    }

    async fn forever(&self) -> impl Stream<Item = u32> {
        futures_util::stream::pending()
    }

    async fn nested(&self) -> impl Stream<Item = Nested> {
        futures_util::stream::iter([Nested])
    }
}

pub type AppSchema = Schema<Query, Mutation, Subscriptions>;

pub fn schema() -> AppSchema {
    Schema::build(Query, Mutation, Subscriptions)
        .data(Notes(Mutex::new(vec!["first".into(), "second".into()])))
        .finish()
}

pub fn query_only_schema() -> Schema<Query, Mutation, EmptySubscription> {
    Schema::build(Query, Mutation, EmptySubscription)
        .data(Notes::default())
        .finish()
}

/// A plugin with code-supplied config (no files read), development defaults.
pub fn plugin() -> GraphqlPlugin<AppSchema> {
    GraphqlPlugin::new(schema())
        .config(GraphqlConfig::default())
        .development(true)
}

pub fn client(plugin: GraphqlPlugin<AppSchema>) -> TestClient {
    TestApp::new().plugin(plugin).build()
}

/// POST `{query, variables}` to `/graphql` and parse the JSON body.
pub async fn post(client: &TestClient, query: &str, variables: Value) -> (u16, Value) {
    let response = client
        .post("/graphql")
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await;
    let status = response.status.as_u16();
    (
        status,
        serde_json::from_slice(&response.body).unwrap_or(Value::Null),
    )
}

pub fn code(body: &Value) -> Option<&str> {
    body["errors"][0]["extensions"]["code"].as_str()
}

pub fn header<'a>(response: &'a autumn_web::test::TestResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}
