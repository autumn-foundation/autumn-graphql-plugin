//! Helpers for testing a mounted endpoint through
//! [`autumn_web::test::TestClient`] (feature `test-support`).
//!
//! ```rust,ignore
//! use autumn_plugin_graphql::testing::GraphqlTestExt as _;
//!
//! let client = TestApp::new().plugin(GraphqlPlugin::new(schema)).build();
//! let res = client.graphql("{ notes { id } }").send().await;
//! res.assert_no_errors();
//! assert_eq!(res.data()["notes"].as_array().unwrap().len(), 2);
//! ```

use autumn_web::test::TestClient;
use serde_json::{Value, json};

/// Adds `.graphql(query)` to [`TestClient`].
pub trait GraphqlTestExt {
    /// Start a GraphQL `POST` to `/graphql`.
    fn graphql(&self, query: &str) -> GraphqlTestRequest<'_>;
}

impl GraphqlTestExt for TestClient {
    fn graphql(&self, query: &str) -> GraphqlTestRequest<'_> {
        GraphqlTestRequest {
            client: self,
            path: "/graphql".to_owned(),
            body: json!({ "query": query }),
            headers: Vec::new(),
        }
    }
}

/// A GraphQL request under construction.
#[must_use]
pub struct GraphqlTestRequest<'a> {
    client: &'a TestClient,
    path: String,
    body: Value,
    headers: Vec<(String, String)>,
}

impl GraphqlTestRequest<'_> {
    /// Send to a different mount path.
    pub fn at(mut self, path: &str) -> Self {
        path.clone_into(&mut self.path);
        self
    }

    /// Set `variables`.
    pub fn variables(mut self, variables: Value) -> Self {
        self.body["variables"] = variables;
        self
    }

    /// Set `operationName`.
    pub fn operation_name(mut self, name: &str) -> Self {
        self.body["operationName"] = Value::from(name);
        self
    }

    /// Set `extensions`.
    pub fn extensions(mut self, extensions: Value) -> Self {
        self.body["extensions"] = extensions;
        self
    }

    /// Add a request header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Send the request.
    pub async fn send(self) -> GraphqlTestResponse {
        let mut request = self.client.post(&self.path).json(&self.body);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = request.send().await;
        let status = response.status.as_u16();
        let body: Value = serde_json::from_str(&response.text()).unwrap_or(Value::Null);
        GraphqlTestResponse { status, body }
    }
}

/// A GraphQL response, with assertion helpers.
#[derive(Debug, Clone)]
pub struct GraphqlTestResponse {
    /// HTTP status.
    pub status: u16,
    /// Parsed JSON body (`null` when the body was not JSON).
    pub body: Value,
}

// Assertion helpers return `&Self` for chaining; ignoring it is fine.
#[allow(clippy::must_use_candidate)]
impl GraphqlTestResponse {
    /// `body.data`.
    #[must_use]
    pub fn data(&self) -> &Value {
        &self.body["data"]
    }

    /// `body.errors` (empty when absent).
    #[must_use]
    pub fn errors(&self) -> Vec<Value> {
        self.body["errors"].as_array().cloned().unwrap_or_default()
    }

    /// Every `errors[].extensions.code`.
    #[must_use]
    pub fn error_codes(&self) -> Vec<String> {
        self.errors()
            .iter()
            .filter_map(|e| e["extensions"]["code"].as_str().map(str::to_owned))
            .collect()
    }

    /// Assert the response carries no errors.
    ///
    /// # Panics
    ///
    /// When `errors` is non-empty.
    #[allow(clippy::panic)]
    pub fn assert_no_errors(&self) -> &Self {
        assert!(
            self.errors().is_empty(),
            "expected no GraphQL errors, got: {}",
            self.body
        );
        self
    }

    /// Assert some error carries `code`.
    ///
    /// # Panics
    ///
    /// When no error has that code.
    pub fn assert_error_code(&self, code: &str) -> &Self {
        assert!(
            self.error_codes().iter().any(|c| c == code),
            "expected an error with code {code}, got: {}",
            self.body
        );
        self
    }

    /// Assert the HTTP status.
    ///
    /// # Panics
    ///
    /// On a different status.
    pub fn assert_status(&self, status: u16) -> &Self {
        assert_eq!(
            self.status, status,
            "unexpected status; body: {}",
            self.body
        );
        self
    }
}
