//! Error vocabulary and mapping.
//!
//! A GraphQL response is an HTTP `200` (under `application/json`), so it
//! never passes through Autumn's problem-details filter, which is where
//! server-side detail is normally redacted. This module is that filter for
//! GraphQL:
//!
//! - [`IntoGraphqlError`] / [`GraphqlResultExt::gql`] turn an
//!   [`AutumnError`] into a field error. Client errors (`4xx`) keep their
//!   message; server errors (`5xx`) are logged and replaced by a generic
//!   message. The HTTP status travels in `extensions.status` and a stable
//!   machine-readable [`code`](codes) in `extensions.code`, so a client can
//!   tell a `422` from a `503` without an HTTP status to read.
//! - Validation failures carry their per-field messages in
//!   `extensions.fields` as well as in the message.
//! - Errors a resolver produced *without* a code (a bare `?` on some
//!   library error, `ctx.data::<T>()?`, …) are "unexpected". When
//!   `mask_unexpected_errors` is on, the plugin replaces their message
//!   before the response leaves the process. Give an error a code to show
//!   its message to clients.

use std::collections::BTreeMap;

use async_graphql::{ErrorExtensions, ServerError, Value};
use autumn_web::AutumnError;
use autumn_web::middleware::AutumnErrorInfo;
use axum::response::IntoResponse;
use http::StatusCode;

/// Stable values for `errors[].extensions.code`.
///
/// Names follow the conventions of Apollo Server and GraphQL Yoga where one
/// exists, so off-the-shelf clients recognise them.
pub mod codes {
    /// `400` — malformed input from the client.
    pub const BAD_USER_INPUT: &str = "BAD_USER_INPUT";
    /// `401` — no or invalid credentials.
    pub const UNAUTHENTICATED: &str = "UNAUTHENTICATED";
    /// `403` — authenticated but not allowed.
    pub const FORBIDDEN: &str = "FORBIDDEN";
    /// `404` — the addressed entity does not exist.
    pub const NOT_FOUND: &str = "NOT_FOUND";
    /// `409` — a conflicting concurrent change.
    pub const CONFLICT: &str = "CONFLICT";
    /// `422` — the model's validation rules rejected the input.
    pub const VALIDATION_FAILED: &str = "VALIDATION_FAILED";
    /// `429` — rate limited.
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    /// Any other `4xx`.
    pub const CLIENT_ERROR: &str = "CLIENT_ERROR";
    /// `503` — a dependency is unavailable; retrying later may succeed.
    pub const SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";
    /// Any other `5xx`, and every masked unexpected error.
    pub const INTERNAL_SERVER_ERROR: &str = "INTERNAL_SERVER_ERROR";
    /// The document is not syntactically valid GraphQL.
    pub const GRAPHQL_PARSE_FAILED: &str = "GRAPHQL_PARSE_FAILED";
    /// The document does not validate against the schema.
    pub const GRAPHQL_VALIDATION_FAILED: &str = "GRAPHQL_VALIDATION_FAILED";
    /// The HTTP request itself is malformed (bad JSON, missing `query`, …).
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    /// The document exceeds a configured static-analysis limit
    /// (`extensions.limit`, `extensions.max`, `extensions.actual` say which).
    pub const QUERY_LIMIT_EXCEEDED: &str = "QUERY_LIMIT_EXCEEDED";
    /// `__schema`/`__type` was selected while introspection is off.
    pub const INTROSPECTION_DISABLED: &str = "INTROSPECTION_DISABLED";
    /// The operation did not finish within `timeout_ms`.
    pub const OPERATION_TIMEOUT: &str = "OPERATION_TIMEOUT";
    /// A hash-only request named a document the server does not have.
    pub const PERSISTED_QUERY_NOT_FOUND: &str = "PERSISTED_QUERY_NOT_FOUND";
    /// A hash-only request reached a server with persisted queries off.
    pub const PERSISTED_QUERY_NOT_SUPPORTED: &str = "PERSISTED_QUERY_NOT_SUPPORTED";
    /// The document text does not hash to the hash the client sent.
    pub const PERSISTED_QUERY_HASH_MISMATCH: &str = "PERSISTED_QUERY_HASH_MISMATCH";
    /// Trusted-documents mode refused a document not in the manifest.
    pub const OPERATION_NOT_ALLOWLISTED: &str = "OPERATION_NOT_ALLOWLISTED";
    /// A mutation or subscription was sent over `GET`.
    pub const METHOD_NOT_ALLOWED: &str = "METHOD_NOT_ALLOWED";
    /// Batching is off, or the batch is larger than `max_operations`.
    pub const BATCH_REJECTED: &str = "BATCH_REJECTED";
    /// The request body is larger than allowed.
    pub const PAYLOAD_TOO_LARGE: &str = "PAYLOAD_TOO_LARGE";
    /// The request's `Content-Type` is not one this endpoint accepts.
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "UNSUPPORTED_MEDIA_TYPE";
    /// The request's `Accept` header names no media type this endpoint serves.
    pub const NOT_ACCEPTABLE: &str = "NOT_ACCEPTABLE";
    /// A multipart request without a preflight-forcing header.
    pub const CSRF_PREVENTION: &str = "CSRF_PREVENTION";
    /// The endpoint is at its concurrent-connection limit.
    pub const TOO_MANY_CONNECTIONS: &str = "TOO_MANY_CONNECTIONS";
}

/// The generic message a redacted or masked error carries.
pub const INTERNAL_MESSAGE: &str = "Internal server error";

/// The `extensions.code` for an HTTP status.
#[must_use]
pub fn code_for_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => codes::BAD_USER_INPUT,
        401 => codes::UNAUTHENTICATED,
        403 => codes::FORBIDDEN,
        404 => codes::NOT_FOUND,
        409 => codes::CONFLICT,
        422 => codes::VALIDATION_FAILED,
        429 => codes::RATE_LIMITED,
        503 => codes::SERVICE_UNAVAILABLE,
        s if (400..500).contains(&s) => codes::CLIENT_ERROR,
        _ => codes::INTERNAL_SERVER_ERROR,
    }
}

/// Build a coded GraphQL error: `extensions.code` and, when given,
/// `extensions.status`.
///
/// ```
/// use autumn_plugin_graphql::error::{codes, graphql_error};
///
/// let err = graphql_error(codes::NOT_FOUND, "no such note", Some(404));
/// assert_eq!(err.message, "no such note");
/// ```
#[must_use]
pub fn graphql_error(
    code: &str,
    message: impl Into<String>,
    status: Option<u16>,
) -> async_graphql::Error {
    let code = code.to_owned();
    async_graphql::Error::new(message).extend_with(move |_, e| {
        e.set("code", code.clone());
        if let Some(status) = status {
            e.set("status", status);
        }
    })
}

/// Conversion into a coded, redaction-safe GraphQL error.
pub trait IntoGraphqlError {
    /// Convert, logging and redacting server-side detail.
    fn into_graphql_error(self) -> async_graphql::Error;
}

impl IntoGraphqlError for AutumnError {
    fn into_graphql_error(self) -> async_graphql::Error {
        // `into_response` is the one place core finalises an error: it applies
        // its own status remapping (a cancelled statement becomes a 503) and
        // stashes the status, message and field details as
        // `AutumnErrorInfo`, which is readable without parsing the body.
        let response = self.into_response();
        let info = response.extensions().get::<AutumnErrorInfo>().cloned();
        let (status, message, details) = info.map_or_else(
            || (response.status(), String::new(), None),
            |info| (info.status, info.message, info.details),
        );
        coded_error(status, &message, details.as_ref())
    }
}

fn coded_error(
    status: StatusCode,
    message: &str,
    details: Option<&std::collections::HashMap<String, Vec<String>>>,
) -> async_graphql::Error {
    let code = code_for_status(status);
    if status.is_server_error() || !status.is_client_error() {
        tracing::error!(status = %status, error = %message, "GraphQL resolver failed");
        return graphql_error(code, INTERNAL_MESSAGE, Some(status.as_u16()));
    }
    let fields: Option<BTreeMap<&String, &Vec<String>>> = details
        .filter(|d| !d.is_empty())
        .map(|d| d.iter().collect());
    let text = fields.as_ref().map_or_else(
        || message.to_owned(),
        |fields| {
            // A validation error's own message is the summary "Validation
            // failed"; the per-field reasons are what a client can act on, and
            // `message` is the one channel every client displays. Sorted (the
            // map is a BTreeMap) so the text is stable.
            fields
                .iter()
                .flat_map(|(field, reasons)| reasons.iter().map(move |r| format!("{field}: {r}")))
                .collect::<Vec<_>>()
                .join("; ")
        },
    );
    let field_map = fields.map(|fields| {
        Value::Object(
            fields
                .into_iter()
                .map(|(field, reasons)| {
                    (
                        async_graphql::Name::new(field),
                        Value::List(reasons.iter().map(|r| Value::from(r.as_str())).collect()),
                    )
                })
                .collect(),
        )
    });
    graphql_error(code, text, Some(status.as_u16())).extend_with(move |_, e| {
        if let Some(fields) = &field_map {
            e.set("fields", fields.clone());
        }
    })
}

/// `?`-friendly conversion for resolver results.
///
/// ```ignore
/// async fn note(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<Option<Note>> {
///     repo(ctx)?.find_by_id(parse_id(&id)?).await.gql()
/// }
/// ```
pub trait GraphqlResultExt<T> {
    /// Map the error side through [`IntoGraphqlError`].
    ///
    /// # Errors
    ///
    /// Returns the converted error when `self` is `Err`.
    fn gql(self) -> async_graphql::Result<T>;
}

impl<T, E: IntoGraphqlError> GraphqlResultExt<T> for Result<T, E> {
    fn gql(self) -> async_graphql::Result<T> {
        self.map_err(IntoGraphqlError::into_graphql_error)
    }
}

/// Read `extensions.code` off a server error, if any.
#[must_use]
pub fn error_code(error: &ServerError) -> Option<&str> {
    match error.extensions.as_ref()?.get("code")? {
        Value::String(code) => Some(code.as_str()),
        _ => None,
    }
}

/// Whether `error` was raised during execution (it has a response path)
/// rather than while parsing or validating the document.
#[must_use]
pub const fn is_execution_error(error: &ServerError) -> bool {
    !error.path.is_empty()
}

/// Build a pre-execution [`ServerError`] carrying `code` and `status`, plus
/// any extra extension values.
#[must_use]
pub fn request_error(
    code: &str,
    message: impl Into<String>,
    status: StatusCode,
    extra: &[(&str, Value)],
) -> ServerError {
    let mut error = ServerError::new(message, None);
    let mut extensions = async_graphql::ErrorExtensionValues::default();
    extensions.set("code", code);
    extensions.set("status", status.as_u16());
    for (key, value) in extra {
        extensions.set(key, value.clone());
    }
    error.extensions = Some(extensions);
    error
}

/// Post-process a response's errors: give every error a code, and mask
/// unexpected execution errors when `mask` is on.
///
/// `parsed` says whether the document parsed; it decides whether an
/// uncoded pre-execution error is a parse or a validation failure.
/// Returns the codes of every error, for metrics.
pub fn finalize_errors(errors: &mut [ServerError], mask: bool, parsed: bool) -> Vec<String> {
    let mut seen = Vec::with_capacity(errors.len());
    for error in errors.iter_mut() {
        if let Some(code) = error_code(error) {
            seen.push(code.to_owned());
            continue;
        }
        let code = if is_execution_error(error) {
            if mask {
                tracing::error!(
                    error = %error.message,
                    path = ?error.path,
                    "masked an unexpected GraphQL resolver error (give it an extensions.code to \
                     expose it)"
                );
                INTERNAL_MESSAGE.clone_into(&mut error.message);
            }
            codes::INTERNAL_SERVER_ERROR
        } else if parsed {
            codes::GRAPHQL_VALIDATION_FAILED
        } else {
            codes::GRAPHQL_PARSE_FAILED
        };
        error
            .extensions
            .get_or_insert_with(Default::default)
            .set("code", code);
        seen.push(code.to_owned());
    }
    seen
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use async_graphql::PathSegment;

    use super::*;

    fn ext(err: &async_graphql::Error, key: &str) -> Option<Value> {
        err.extensions.as_ref()?.get(key).cloned()
    }

    #[test]
    fn client_errors_keep_their_message_and_carry_code_and_status() {
        let err = AutumnError::not_found_msg("note 9 not found").into_graphql_error();
        assert_eq!(err.message, "note 9 not found");
        assert_eq!(ext(&err, "code"), Some(Value::from("NOT_FOUND")));
        assert_eq!(ext(&err, "status"), Some(Value::from(404)));

        let err = AutumnError::unauthorized_msg("sign in").into_graphql_error();
        assert_eq!(ext(&err, "code"), Some(Value::from("UNAUTHENTICATED")));
        let err = AutumnError::forbidden_msg("no").into_graphql_error();
        assert_eq!(ext(&err, "code"), Some(Value::from("FORBIDDEN")));
        let err = AutumnError::bad_request_msg("bad id").into_graphql_error();
        assert_eq!(ext(&err, "code"), Some(Value::from("BAD_USER_INPUT")));
    }

    #[test]
    fn server_errors_are_redacted() {
        let err = AutumnError::internal_server_error_msg("connection refused: 10.0.0.3:5432")
            .into_graphql_error();
        assert_eq!(err.message, INTERNAL_MESSAGE);
        assert_eq!(
            ext(&err, "code"),
            Some(Value::from("INTERNAL_SERVER_ERROR"))
        );
        assert_eq!(ext(&err, "status"), Some(Value::from(500)));

        let err = AutumnError::service_unavailable_msg("no pool").into_graphql_error();
        assert_eq!(err.message, INTERNAL_MESSAGE);
        assert_eq!(ext(&err, "code"), Some(Value::from("SERVICE_UNAVAILABLE")));
        assert_eq!(ext(&err, "status"), Some(Value::from(503)));
    }

    #[test]
    fn validation_details_become_message_and_fields() {
        let mut details = HashMap::new();
        details.insert("title".to_owned(), vec!["too short".to_owned()]);
        details.insert(
            "body".to_owned(),
            vec!["required".to_owned(), "x".to_owned()],
        );
        let err = AutumnError::validation(details).into_graphql_error();
        assert_eq!(err.message, "body: required; body: x; title: too short");
        assert_eq!(ext(&err, "code"), Some(Value::from("VALIDATION_FAILED")));
        assert_eq!(ext(&err, "status"), Some(Value::from(422)));
        let Some(Value::Object(fields)) = ext(&err, "fields") else {
            panic!("fields missing");
        };
        assert_eq!(
            fields.get("title"),
            Some(&Value::List(vec![Value::from("too short")]))
        );
    }

    #[test]
    fn gql_maps_results() {
        let ok: Result<u8, AutumnError> = Ok(1);
        assert_eq!(ok.gql().unwrap(), 1);
        let err: Result<u8, AutumnError> = Err(AutumnError::conflict_msg("stale"));
        let err = err.gql().unwrap_err();
        assert_eq!(ext(&err, "code"), Some(Value::from("CONFLICT")));
    }

    #[test]
    fn status_codes_map_to_stable_codes() {
        assert_eq!(
            code_for_status(StatusCode::TOO_MANY_REQUESTS),
            codes::RATE_LIMITED
        );
        assert_eq!(code_for_status(StatusCode::GONE), codes::CLIENT_ERROR);
        assert_eq!(
            code_for_status(StatusCode::BAD_GATEWAY),
            codes::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            code_for_status(StatusCode::OK),
            codes::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn finalize_masks_only_uncoded_execution_errors() {
        let mut unexpected = ServerError::new("Data `Pool` does not exist.", None);
        unexpected.path = vec![PathSegment::Field("notes".into())];
        let mut coded = ServerError::new("note 2 is pinned", None);
        coded.path = vec![PathSegment::Field("deleteNote".into())];
        let mut ext = async_graphql::ErrorExtensionValues::default();
        ext.set("code", "VALIDATION_FAILED");
        coded.extensions = Some(ext);
        let validation = ServerError::new("Unknown field \"nope\"", None);

        let mut errors = vec![unexpected, coded, validation];
        let seen = finalize_errors(&mut errors, true, true);
        assert_eq!(errors[0].message, INTERNAL_MESSAGE);
        assert_eq!(errors[1].message, "note 2 is pinned");
        assert_eq!(errors[2].message, "Unknown field \"nope\"");
        assert_eq!(
            seen,
            [
                "INTERNAL_SERVER_ERROR",
                "VALIDATION_FAILED",
                "GRAPHQL_VALIDATION_FAILED"
            ]
        );

        let mut unmasked = vec![ServerError::new("boom", None)];
        unmasked[0].path = vec![PathSegment::Index(0)];
        finalize_errors(&mut unmasked, false, true);
        assert_eq!(unmasked[0].message, "boom");
        assert_eq!(error_code(&unmasked[0]), Some("INTERNAL_SERVER_ERROR"));

        let mut parse = vec![ServerError::new(" --> 1:1 expected selection", None)];
        finalize_errors(&mut parse, true, false);
        assert_eq!(error_code(&parse[0]), Some("GRAPHQL_PARSE_FAILED"));
    }

    #[test]
    fn request_errors_carry_extras() {
        let err = request_error(
            codes::QUERY_LIMIT_EXCEEDED,
            "too deep",
            StatusCode::BAD_REQUEST,
            &[("limit", Value::from("depth"))],
        );
        assert_eq!(error_code(&err), Some("QUERY_LIMIT_EXCEEDED"));
        assert_eq!(
            err.extensions.as_ref().unwrap().get("limit"),
            Some(&Value::from("depth"))
        );
        assert!(!is_execution_error(&err));
    }
}
