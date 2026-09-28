//! Automatic Persisted Queries and trusted documents.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use autumn_plugin_graphql::persisted::sha256_hex;
use autumn_plugin_graphql::{GraphqlConfig, PersistedQueryMode, TrustedDocuments};
use autumn_web::test::TestClient;
use common::{code, plugin};
use serde_json::{Value, json};

const DOC: &str = "{ cached }";

async fn send(client: &TestClient, body: Value) -> (u16, Value) {
    let response = client.post("/graphql").json(&body).send().await;
    (response.status.as_u16(), response.json())
}

fn apq(hash: &str) -> Value {
    json!({ "persistedQuery": { "version": 1, "sha256Hash": hash } })
}

#[tokio::test]
async fn hash_only_requests_are_refused_when_persisted_queries_are_off() {
    let client = common::client(plugin());
    let (status, body) = send(&client, json!({ "extensions": apq(&sha256_hex(DOC)) })).await;
    assert_eq!(status, 200);
    assert_eq!(code(&body), Some("PERSISTED_QUERY_NOT_SUPPORTED"));
    // With the document, the extension is simply ignored.
    let (_, body) = send(
        &client,
        json!({ "query": DOC, "extensions": apq("0".repeat(64).as_str()) }),
    )
    .await;
    assert_eq!(body["data"]["cached"], 7, "{body}");
}

#[tokio::test]
async fn automatic_persisted_queries_follow_the_apollo_protocol() {
    let client = common::client(plugin().persisted_queries(PersistedQueryMode::Automatic));
    let hash = sha256_hex(DOC);

    // 1. Hash only: a miss.
    let (_, body) = send(&client, json!({ "extensions": apq(&hash) })).await;
    assert_eq!(code(&body), Some("PERSISTED_QUERY_NOT_FOUND"));
    assert_eq!(body["errors"][0]["message"], "PersistedQueryNotFound");

    // 2. Hash + document: verified, registered, executed.
    let (_, body) = send(&client, json!({ "query": DOC, "extensions": apq(&hash) })).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");

    // 3. Hash only: a hit, over GET too.
    let (_, body) = send(&client, json!({ "extensions": apq(&hash) })).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");
    let ext = urlencode(&apq(&hash).to_string());
    let response = client
        .get(&format!("/graphql?extensions={ext}"))
        .send()
        .await;
    assert_eq!(response.json::<Value>()["data"]["cached"], 7);

    // A document that does not hash to the claimed hash is refused.
    let (_, body) = send(
        &client,
        json!({ "query": "{ profile }", "extensions": apq(&hash) }),
    )
    .await;
    assert_eq!(code(&body), Some("PERSISTED_QUERY_HASH_MISMATCH"));

    // Malformed extensions are request errors.
    let (status, body) = send(
        &client,
        json!({ "query": DOC, "extensions": { "persistedQuery": { "version": 2, "sha256Hash": hash } } }),
    )
    .await;
    assert_eq!((status, code(&body)), (200, Some("BAD_REQUEST")));
    let (_, body) = send(&client, json!({ "extensions": apq("not-a-hash") })).await;
    assert_eq!(code(&body), Some("BAD_REQUEST"));

    // Free-form documents still work in automatic mode.
    let (_, body) = send(&client, json!({ "query": "{ profile }" })).await;
    assert!(body["data"]["profile"].is_string(), "{body}");
}

#[tokio::test]
async fn persisted_lookups_still_pass_the_limits() {
    let deep = "{ nested { nested { nested { value } } } }";
    let client = common::client(
        plugin()
            .persisted_queries(PersistedQueryMode::Automatic)
            .configure(|c| c.limits.max_depth = 2),
    );
    let hash = sha256_hex(deep);
    let (_, body) = send(&client, json!({ "query": deep, "extensions": apq(&hash) })).await;
    assert_eq!(code(&body), Some("QUERY_LIMIT_EXCEEDED"));
    let (_, body) = send(&client, json!({ "extensions": apq(&hash) })).await;
    assert_eq!(
        code(&body),
        Some("QUERY_LIMIT_EXCEEDED"),
        "cached, then re-checked: {body}"
    );
}

#[tokio::test]
async fn trusted_documents_only_run_what_the_manifest_allows() {
    let mut documents = TrustedDocuments::new().with_document(DOC);
    documents.insert(
        "create-note",
        "mutation { createNote(title: \"t\") { id } }",
    );
    let client = common::client(
        plugin()
            .persisted_queries(PersistedQueryMode::Trusted)
            .trusted_documents(documents),
    );

    // By APQ hash, by documentId (with and without the sha256: prefix), by text.
    let hash = sha256_hex(DOC);
    let (_, body) = send(&client, json!({ "extensions": apq(&hash) })).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");
    let (_, body) = send(&client, json!({ "documentId": format!("sha256:{hash}") })).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");
    let (_, body) = send(&client, json!({ "documentId": "create-note" })).await;
    assert!(body["data"]["createNote"]["id"].is_number(), "{body}");
    let (_, body) = send(&client, json!({ "query": DOC })).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");

    // Anything else is refused — including introspection.
    let (_, body) = send(&client, json!({ "query": "{ profile }" })).await;
    assert_eq!(code(&body), Some("OPERATION_NOT_ALLOWLISTED"));
    let (_, body) = send(
        &client,
        json!({ "query": "{ __schema { types { name } } }" }),
    )
    .await;
    assert_eq!(code(&body), Some("OPERATION_NOT_ALLOWLISTED"));
    let (_, body) = send(&client, json!({ "documentId": "unknown" })).await;
    assert_eq!(code(&body), Some("PERSISTED_QUERY_NOT_FOUND"));
    // A known id with a different document is refused, never executed.
    let (_, body) = send(
        &client,
        json!({ "documentId": "create-note", "query": "{ profile }" }),
    )
    .await;
    assert_eq!(code(&body), Some("OPERATION_NOT_ALLOWLISTED"));

    // Under the graphql-response+json media type the refusal is a 403.
    let response = client
        .post("/graphql")
        .header("accept", "application/graphql-response+json")
        .json(&json!({ "query": "{ profile }" }))
        .send()
        .await;
    assert_eq!(response.status, 403);
}

#[tokio::test]
async fn a_manifest_file_is_loaded_relative_to_the_config() {
    let dir = std::env::temp_dir().join(format!("autumn-graphql-manifest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("persisted-query-manifest.json");
    std::fs::write(
        &manifest,
        json!({
            "format": "apollo-persisted-query-manifest",
            "version": 1,
            "operations": [{ "id": "cached-v1", "name": "Cached", "type": "query", "body": DOC }]
        })
        .to_string(),
    )
    .unwrap();
    let mut config = GraphqlConfig::default();
    config.persisted_queries.mode = PersistedQueryMode::Trusted;
    config.persisted_queries.manifest = manifest.display().to_string();
    let client = common::client(
        autumn_plugin_graphql::GraphqlPlugin::new(common::schema())
            .config(config)
            .development(true),
    );
    let (_, body) = send(&client, json!({ "documentId": "cached-v1" })).await;
    assert_eq!(body["data"]["cached"], 7, "{body}");
    let _ = std::fs::remove_dir_all(dir);
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
