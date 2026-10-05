//! HTTP transport: GraphQL-over-HTTP semantics, hardening and context.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use autumn_plugin_graphql::{GraphqlConfig, GraphqlPlugin};
use autumn_web::test::TestApp;
use common::{client, code, header, plugin, post, query_only_schema};
use serde_json::{Value, json};

// ── Basic execution ─────────────────────────────────────────────────────────

#[tokio::test]
async fn post_executes_queries_and_mutations_with_app_state() {
    let client = client(plugin());
    let (status, body) = post(&client, "{ notes { id title } profile }", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["notes"][1]["title"], "second");
    assert!(
        body["data"]["profile"].is_string(),
        "AppState reached the resolver: {body}"
    );

    let (_, body) = post(
        &client,
        "mutation($t: String!) { createNote(title: $t) { id title } }",
        json!({ "t": "third" }),
    )
    .await;
    assert_eq!(body["data"]["createNote"]["title"], "third", "{body}");
}

#[tokio::test]
async fn get_serves_queries_and_refuses_mutations_with_405() {
    let client = client(plugin());
    let response = client.get("/graphql?query=%7B%20cached%20%7D").send().await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json::<Value>()["data"]["cached"], 7);
    assert_eq!(header(&response, "cache-control"), Some("max-age=60"));
    assert_eq!(header(&response, "vary"), Some("Accept"));

    let mutation = "mutation%20%7B%20createNote(title%3A%20%22x%22)%20%7B%20id%20%7D%20%7D";
    let response = client
        .get(&format!("/graphql?query={mutation}"))
        .send()
        .await;
    assert_eq!(response.status, 405);
    let body: Value = response.json();
    assert_eq!(code(&body), Some("METHOD_NOT_ALLOWED"));
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("mutation")
    );

    // A named mutation in a mixed document is selected by operationName.
    let doc = "query%20A%20%7B%20cached%20%7D%20mutation%20B%20%7B%20createNote(title%3A%22x%22)%20%7B%20id%20%7D%20%7D";
    assert_eq!(
        client
            .get(&format!("/graphql?query={doc}&operationName=B"))
            .send()
            .await
            .status,
        405
    );
    assert_eq!(
        client
            .get(&format!("/graphql?query={doc}&operationName=A"))
            .send()
            .await
            .status,
        200
    );

    // Nothing executed: the note list is unchanged.
    let (_, body) = post(&client, "{ notes { id } }", json!({})).await;
    assert_eq!(body["data"]["notes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn get_without_a_query_is_a_400_and_can_be_disabled() {
    let client = client(plugin());
    assert_eq!(client.get("/graphql").send().await.status, 400);

    let client = client_with(|c| c.allow_get = false);
    let response = client.get("/graphql?query=%7B%20cached%20%7D").send().await;
    assert_eq!(response.status, 405);
}

#[tokio::test]
async fn post_bodies_are_validated() {
    let client = client(plugin());
    let response = client
        .post("/graphql")
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await;
    assert_eq!(response.status, 400);
    assert_eq!(code(&response.json()), Some("BAD_REQUEST"));

    let response = client
        .post("/graphql")
        .header("content-type", "text/plain")
        .body("{ cached }")
        .send()
        .await;
    assert_eq!(response.status, 415);
    assert_eq!(code(&response.json()), Some("UNSUPPORTED_MEDIA_TYPE"));

    let response = client
        .post("/graphql")
        .header("content-type", "application/graphql")
        .body("{ cached }")
        .send()
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json::<Value>()["data"]["cached"], 7);
}

#[tokio::test]
async fn oversized_bodies_are_413() {
    let client = client_with(|c| c.max_body_bytes = 64);
    let query = format!("{{ notes {{ id }} }} #{}", "x".repeat(200));
    let (status, body) = post(&client, &query, json!({})).await;
    assert_eq!(status, 413, "{body}");
    assert_eq!(code(&body), Some("PAYLOAD_TOO_LARGE"));
}

// ── Content negotiation (GraphQL over HTTP) ─────────────────────────────────

#[tokio::test]
async fn graphql_response_json_uses_meaningful_status_codes() {
    let client = client(plugin());
    let send = |accept: &'static str, query: &'static str| {
        let client = &client;
        async move {
            client
                .post("/graphql")
                .header("accept", accept)
                .json(&json!({ "query": query }))
                .send()
                .await
        }
    };

    let ok = send("application/graphql-response+json", "{ cached }").await;
    assert_eq!(ok.status, 200);
    assert!(
        header(&ok, "content-type")
            .unwrap()
            .starts_with("application/graphql-response+json")
    );

    // Validation failure: a request error → 400 under the new media type...
    let invalid = send("application/graphql-response+json", "{ nope }").await;
    assert_eq!(invalid.status, 400);
    assert_eq!(code(&invalid.json()), Some("GRAPHQL_VALIDATION_FAILED"));
    // ...but 200 under legacy application/json.
    let legacy = send("application/json", "{ nope }").await;
    assert_eq!(legacy.status, 200);
    assert!(
        header(&legacy, "content-type")
            .unwrap()
            .starts_with("application/json")
    );

    let syntax = send("application/graphql-response+json", "{ cached").await;
    assert_eq!(syntax.status, 400);
    assert_eq!(code(&syntax.json()), Some("GRAPHQL_PARSE_FAILED"));

    // A field error after execution started is still a 200 (partial data).
    let field_error = send("application/graphql-response+json", "{ cached coded }").await;
    assert_eq!(field_error.status, 200);

    let unacceptable = send("text/html", "{ cached }").await;
    assert_eq!(unacceptable.status, 406);
}

// ── Batching ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn batching_is_off_by_default_and_bounded_when_on() {
    let batch = json!([{ "query": "{ cached }" }, { "query": "mutation { createNote(title: \"b\") { id } }" }]);
    let client = client(plugin());
    let response = client.post("/graphql").json(&batch).send().await;
    assert_eq!(response.status, 400);
    assert_eq!(code(&response.json()), Some("BATCH_REJECTED"));

    let client = common::client(plugin().batching(2));
    let response = client.post("/graphql").json(&batch).send().await;
    assert_eq!(response.status, 200);
    let body: Value = response.json();
    assert_eq!(body[0]["data"]["cached"], 7);
    assert_eq!(body[1]["data"]["createNote"]["id"], 2, "sequential: {body}");

    let too_many =
        json!([{ "query": "{ cached }" }, { "query": "{ cached }" }, { "query": "{ cached }" }]);
    let response = client.post("/graphql").json(&too_many).send().await;
    assert_eq!(response.status, 400);
    assert_eq!(code(&response.json()), Some("BATCH_REJECTED"));
}

// ── Static limits ───────────────────────────────────────────────────────────

#[tokio::test]
async fn query_limits_are_enforced_before_execution() {
    let client = client_with(|c| {
        c.limits.max_depth = 3;
        c.limits.max_aliases = 2;
        c.limits.max_root_fields = 3;
        c.limits.max_fields = 6;
        c.limits.max_nesting = 8;
        c.limits.max_query_bytes = 400;
    });
    let cases = [
        ("{ nested { nested { nested { value } } } }", "depth"),
        ("{ a: cached b: cached c: cached }", "aliases"),
        ("{ cached profile transport notes { id } }", "root_fields"),
        (
            "{ nested { value nested { value nested { value } } } }",
            "depth",
        ),
        (
            "{ ...F ...F } fragment F on Query { nested { value nested { value } } }",
            "fields",
        ),
        ("{ nested(x: [[[[[[[[1]]]]]]]]) { value } }", "nesting"),
    ];
    for (query, limit) in cases {
        let (status, body) = post(&client, query, json!({})).await;
        assert_eq!(status, 200, "{query}: {body}");
        assert_eq!(code(&body), Some("QUERY_LIMIT_EXCEEDED"), "{query}: {body}");
        assert_eq!(
            body["errors"][0]["extensions"]["limit"], limit,
            "{query}: {body}"
        );
        assert!(body["data"].is_null(), "nothing executed: {body}");
    }
    let (_, body) = post(
        &client,
        &format!("{{ cached }} #{}", "x".repeat(500)),
        json!({}),
    )
    .await;
    assert_eq!(body["errors"][0]["extensions"]["limit"], "query_bytes");

    let (_, body) = post(&client, "{ nested { nested { value } } }", json!({})).await;
    assert!(body.get("errors").is_none(), "within limits: {body}");
}

#[tokio::test]
async fn schema_level_complexity_applies_with_from_builder() {
    let builder =
        async_graphql::Schema::build(common::Query, common::Mutation, common::Subscriptions)
            .data(common::Notes::default());
    let plugin = GraphqlPlugin::from_builder(builder)
        .config(GraphqlConfig::default())
        .configure(|c| c.limits.max_complexity = 2)
        .development(true);
    let client = TestApp::new().plugin(plugin).build();
    let (_, body) = post(&client, "{ cached profile transport }", json!({})).await;
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("complex"),
        "{body}"
    );
    let (_, body) = post(&client, "{ cached }", json!({})).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");
}

// ── Introspection, SDL, masking ─────────────────────────────────────────────

#[tokio::test]
async fn production_defaults_hide_the_schema_and_mask_unexpected_errors() {
    let client = common::client(
        GraphqlPlugin::new(common::schema())
            .config(GraphqlConfig::default())
            .development(false),
    );
    let (_, body) = post(&client, "{ __schema { queryType { name } } }", json!({})).await;
    assert_eq!(code(&body), Some("INTROSPECTION_DISABLED"), "{body}");
    assert!(body["data"].is_null());
    let (_, body) = post(&client, "{ __typename }", json!({})).await;
    assert_eq!(
        body["data"]["__typename"], "Query",
        "__typename is always allowed"
    );
    assert_eq!(client.get("/graphql/sdl").send().await.status, 404);

    let (_, body) = post(&client, "{ boom }", json!({})).await;
    assert_eq!(body["errors"][0]["message"], "Internal server error");
    assert_eq!(code(&body), Some("INTERNAL_SERVER_ERROR"));
    assert!(!body.to_string().contains("hunter2"), "{body}");

    let (_, body) = post(&client, "{ coded }", json!({})).await;
    assert_eq!(
        body["errors"][0]["message"], "slow down",
        "coded errors are shown"
    );
}

#[tokio::test]
async fn development_defaults_show_the_schema_and_errors() {
    let client = client(plugin());
    let (_, body) = post(&client, "{ __schema { queryType { name } } }", json!({})).await;
    assert_eq!(body["data"]["__schema"]["queryType"]["name"], "Query");
    let sdl = client.get("/graphql/sdl").send().await;
    assert_eq!(sdl.status, 200);
    assert!(sdl.text().contains("type Query"));

    let (_, body) = post(&client, "{ boom }", json!({})).await;
    assert_eq!(body["errors"][0]["message"], "database password is hunter2");
    assert_eq!(code(&body), Some("INTERNAL_SERVER_ERROR"), "still coded");
}

#[tokio::test]
async fn autumn_errors_map_to_codes_and_redact_server_detail() {
    let client = client(plugin());
    let (_, body) = post(&client, "{ autumnError(status: 404) }", json!({})).await;
    assert_eq!(body["errors"][0]["message"], "note 9 not found");
    assert_eq!(body["errors"][0]["extensions"]["status"], 404);
    assert_eq!(code(&body), Some("NOT_FOUND"));

    let (_, body) = post(&client, "{ autumnError(status: 422) }", json!({})).await;
    assert_eq!(body["errors"][0]["message"], "title: must not be blank");
    assert_eq!(
        body["errors"][0]["extensions"]["fields"]["title"][0],
        "must not be blank"
    );

    let (_, body) = post(&client, "{ autumnError(status: 500) }", json!({})).await;
    assert_eq!(body["errors"][0]["message"], "Internal server error");
    assert!(!body.to_string().contains("sda1"), "{body}");
}

// ── Timeouts ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn slow_operations_time_out() {
    let client = common::client(plugin().timeout(Duration::from_millis(50)));
    let (status, body) = post(&client, "{ slow(ms: 2000) }", json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(code(&body), Some("OPERATION_TIMEOUT"));

    let response = client
        .post("/graphql")
        .header("accept", "application/graphql-response+json")
        .json(&json!({ "query": "{ slow(ms: 2000) }" }))
        .send()
        .await;
    assert_eq!(response.status, 504);

    let (_, body) = post(&client, "{ slow(ms: 1) }", json!({})).await;
    assert_eq!(body["data"]["slow"], true);
}

// ── Context ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn context_hooks_add_data_and_can_refuse_requests() {
    let plugin = plugin().context(|parts, _state, data| {
        Box::pin(async move {
            match parts.headers.get("x-user").and_then(|v| v.to_str().ok()) {
                Some("mallory") => Err(autumn_web::AutumnError::unauthorized_msg("banned")),
                Some(user) => {
                    data.insert(common::Viewer(user.to_owned()));
                    Ok(())
                }
                None => Ok(()),
            }
        })
    });
    let client = common::client(plugin);

    let response = client
        .post("/graphql")
        .header("x-user", "alice")
        .json(&json!({ "query": "{ viewer requestHeader(name: \"x-user\") transport }" }))
        .send()
        .await;
    let body: Value = response.json();
    assert_eq!(body["data"]["viewer"], "alice", "{body}");
    assert_eq!(body["data"]["requestHeader"], "alice");
    assert_eq!(body["data"]["transport"], "http_post");

    let (_, body) = post(&client, "{ viewer }", json!({})).await;
    assert_eq!(body["data"]["viewer"], Value::Null);

    let response = client
        .post("/graphql")
        .header("x-user", "mallory")
        .json(&json!({ "query": "{ viewer }" }))
        .send()
        .await;
    assert_eq!(response.status, 401);
    let body: Value = response.json();
    assert_eq!(code(&body), Some("UNAUTHENTICATED"));
    assert_eq!(body["errors"][0]["message"], "banned");
}

#[tokio::test]
async fn resolver_set_headers_reach_the_response() {
    let client = client(plugin());
    let response = client
        .post("/graphql")
        .json(&json!({ "query": "{ setsHeader }" }))
        .send()
        .await;
    assert_eq!(header(&response, "x-from-resolver"), Some("yes"));
    assert_eq!(
        header(&response, "cache-control"),
        None,
        "POST is never cacheable"
    );
}

// ── Mounting ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn two_endpoints_coexist_and_a_duplicate_path_is_skipped() {
    let client = TestApp::new()
        .plugin(plugin())
        .plugin(plugin().path("/internal/graphql").without_sdl())
        .plugin(plugin()) // same path: skipped as a duplicate
        .build();
    assert_eq!(client.get("/graphql/sdl").send().await.status, 200);
    assert_eq!(client.get("/internal/graphql/sdl").send().await.status, 404);
    for path in ["/graphql", "/internal/graphql"] {
        let response = client
            .post(path)
            .json(&json!({ "query": "{ __typename }" }))
            .send()
            .await;
        assert_eq!(
            response.json::<Value>()["data"]["__typename"],
            "Query",
            "{path}"
        );
    }
}

#[tokio::test]
async fn the_kill_switch_mounts_nothing() {
    let plugin = plugin().configure(|c| c.enabled = false);
    assert!(plugin.route_infos().is_empty());
    let client = common::client(plugin);
    let (status, _) = post(&client, "{ cached }", json!({})).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_guard_protects_every_route() {
    use autumn_web::auth::{InMemoryApiTokenStore, RequireApiToken};
    let store = Arc::new(InMemoryApiTokenStore::default().with_token("s3cret", "tests"));
    let plugin = plugin().guard(RequireApiToken::new(store), "RequireApiToken");
    assert!(plugin.route_infos().iter().all(|r| {
        r.classification == autumn_web::route_listing::RouteClassification::Gated
            && r.middleware == ["RequireApiToken"]
    }));
    let client = common::client(plugin);
    assert_eq!(client.get("/graphql/sdl").send().await.status, 401);
    assert_eq!(post(&client, "{ cached }", json!({})).await.0, 401);
    assert_eq!(client.get("/graphql/ws").send().await.status, 401);
    let response = client
        .post("/graphql")
        .header("authorization", "Bearer s3cret")
        .json(&json!({ "query": "{ cached }" }))
        .send()
        .await;
    assert_eq!(response.status, 200);
}

#[test]
#[should_panic(expected = "trusted")]
fn misconfiguration_aborts_boot() {
    let plugin = plugin().persisted_queries(autumn_plugin_graphql::PersistedQueryMode::Trusted);
    let _ = TestApp::new().plugin(plugin).build();
}

#[test]
fn an_invalid_override_is_reported() {
    let plugin = plugin().path("no-slash");
    assert!(plugin.effective_config().is_err());
}

// ── Uploads ─────────────────────────────────────────────────────────────────

fn multipart_body() -> (String, String) {
    let boundary = "XBOUNDARYX";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"operations\"\r\n\r\n\
         {{\"query\":\"mutation($f: Upload!) {{ upload(file: $f) }}\",\"variables\":{{\"f\":null}}}}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"map\"\r\n\r\n{{\"0\":[\"variables.f\"]}}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"0\"; filename=\"a.txt\"\r\n\
         Content-Type: text/plain\r\n\r\nhello\r\n--{boundary}--\r\n"
    );
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn multipart_uploads_need_opt_in_and_a_preflight_header() {
    let (content_type, body) = multipart_body();

    let client = client(plugin());
    let response = client
        .post("/graphql")
        .header("content-type", &content_type)
        .body(body.clone())
        .send()
        .await;
    assert_eq!(response.status, 415, "uploads are off by default");

    let client = client_with(|c| c.uploads.enabled = true);
    let response = client
        .post("/graphql")
        .header("content-type", &content_type)
        .body(body.clone())
        .send()
        .await;
    assert_eq!(response.status, 400);
    assert_eq!(code(&response.json()), Some("CSRF_PREVENTION"));

    let response = client
        .post("/graphql")
        .header("content-type", &content_type)
        .header("apollo-require-preflight", "true")
        .body(body)
        .send()
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json::<Value>()["data"]["upload"], "a.txt:5");
}

// ── Dynamic schemas ─────────────────────────────────────────────────────────

#[tokio::test]
async fn any_executor_can_be_mounted() {
    use async_graphql::dynamic::{Field, FieldFuture, Object, Schema, TypeRef};
    let query = Object::new("Query").field(Field::new(
        "hello",
        TypeRef::named_nn(TypeRef::STRING),
        |_| FieldFuture::new(async { Ok(Some(async_graphql::Value::from("world"))) }),
    ));
    let schema = Schema::build("Query", None, None)
        .register(query)
        .finish()
        .unwrap();
    let sdl = schema.sdl();
    let plugin = GraphqlPlugin::from_executor(schema)
        .sdl_text(sdl)
        .config(GraphqlConfig::default())
        .development(true);
    let client = TestApp::new().plugin(plugin).build();
    let response = client
        .post("/graphql")
        .json(&json!({ "query": "{ hello }" }))
        .send()
        .await;
    assert_eq!(response.json::<Value>()["data"]["hello"], "world");
    assert!(
        client
            .get("/graphql/sdl")
            .send()
            .await
            .text()
            .contains("hello")
    );
}

#[tokio::test]
async fn a_schema_without_subscriptions_mounts_no_stream_routes() {
    let plugin = GraphqlPlugin::new(query_only_schema())
        .config(GraphqlConfig::default())
        .development(true);
    let routes: Vec<String> = plugin
        .route_infos()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(
        routes,
        ["POST /graphql", "GET /graphql", "GET /graphql/sdl"]
    );
    let client = TestApp::new().plugin(plugin).build();
    let response = client
        .post("/graphql")
        .header("accept", "text/event-stream")
        .json(&json!({ "query": "{ cached }" }))
        .send()
        .await;
    assert_eq!(
        response.status, 406,
        "SSE is off without a subscription root"
    );
}

fn client_with(
    apply: impl Fn(&mut GraphqlConfig) + Send + Sync + 'static,
) -> autumn_web::test::TestClient {
    common::client(plugin().configure(apply))
}

// ── Metrics ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn operations_and_errors_are_counted() {
    let client = common::client(plugin().path("/metrics-probe/graphql"));
    for query in ["{ cached }", "{ coded }", "{ nope }"] {
        client
            .post("/metrics-probe/graphql")
            .json(&json!({ "query": query }))
            .send()
            .await;
    }
    let snapshot = autumn_web::metrics::snapshot();
    let series = |name: &str| -> Vec<std::collections::BTreeMap<String, String>> {
        snapshot
            .iter()
            .filter(|i| i.name == name)
            .flat_map(|i| i.series.iter().map(|s| s.labels.clone()))
            .filter(|labels| {
                labels.get("endpoint").map(String::as_str) == Some("/metrics-probe/graphql")
            })
            .collect()
    };
    let operations = series("graphql_operations_total");
    for outcome in ["success", "error"] {
        assert!(
            operations
                .iter()
                .any(|l| l["outcome"] == outcome && l["operation"] == "query"),
            "{outcome}: {operations:?}"
        );
    }
    let errors = series("graphql_errors_total");
    for code in ["RATE_LIMITED", "GRAPHQL_VALIDATION_FAILED"] {
        assert!(
            errors.iter().any(|l| l["code"] == code),
            "{code}: {errors:?}"
        );
    }
    let durations = series("graphql_operation_duration_seconds");
    assert_ne!(durations.len(), 0, "no duration series: {durations:?}");
}

// ── Test helpers (feature `test-support`) ───────────────────────────────────

#[cfg(feature = "test-support")]
#[tokio::test]
async fn the_test_helpers_drive_an_endpoint() {
    use autumn_plugin_graphql::testing::GraphqlTestExt as _;
    let client = client(plugin());
    let response = client
        .graphql("query Q($t: String!) { cached requestHeader(name: $t) }")
        .variables(json!({ "t": "x-test" }))
        .operation_name("Q")
        .header("x-test", "hi")
        .send()
        .await;
    response.assert_status(200).assert_no_errors();
    assert_eq!(response.data()["requestHeader"], "hi");

    let response = client.graphql("{ coded }").send().await;
    response.assert_error_code("RATE_LIMITED");
    assert_eq!(response.error_codes(), ["RATE_LIMITED"]);

    let response = client
        .graphql("{ cached }")
        .at("/graphql")
        .extensions(json!({}))
        .send()
        .await;
    assert_eq!(response.data()["cached"], 7);
}
