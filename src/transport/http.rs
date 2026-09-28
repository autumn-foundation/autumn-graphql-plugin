//! `POST {path}`, `GET {path}` and `GET {path}/sdl`.

use std::sync::Arc;

use ::http::request::Parts;
use ::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use async_graphql::http::MultipartOptions;
use async_graphql::{Executor, Request, Response as GqlResponse, Variables};
use autumn_web::AppState;
use axum::extract::{Extension, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{Endpoint, Format, json_response, negotiate, plain, rejection_response};
use crate::context::Transport;
use crate::error::codes;
use crate::pipeline::{Executed, Rejection};

/// Headers any of which proves a request was preflighted (or is not a
/// browser "simple request"), per Apollo's CSRF-prevention convention.
const PREFLIGHT_HEADERS: [&str; 3] = [
    "apollo-require-preflight",
    "graphql-require-preflight",
    "x-apollo-operation-name",
];

/// One operation as it arrives on the wire. Parsed by hand (not straight
/// into `async_graphql::Request`) so a malformed body gets a precise
/// message and the `documentId` of the persisted-documents proposal is
/// understood.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRequest {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    operation_name: Option<String>,
    #[serde(default)]
    variables: Option<serde_json::Value>,
    #[serde(default)]
    extensions: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    document_id: Option<String>,
}

/// A request plus its persisted-document id, if it named one.
pub struct Incoming {
    pub request: Request,
    pub document_id: Option<String>,
}

fn bad_request(message: impl Into<String>) -> Rejection {
    Rejection::new(codes::BAD_REQUEST, message, StatusCode::BAD_REQUEST).transport_level()
}

impl WireRequest {
    fn into_incoming(self) -> Result<Incoming, Rejection> {
        let mut request = Request::new(self.query.unwrap_or_default());
        if let Some(name) = self.operation_name.filter(|n| !n.is_empty()) {
            request = request.operation_name(name);
        }
        match self.variables {
            None | Some(serde_json::Value::Null) => {}
            Some(value @ serde_json::Value::Object(_)) => {
                request = request.variables(Variables::from_json(value));
            }
            Some(_) => return Err(bad_request("`variables` must be an object")),
        }
        if let Some(extensions) = self.extensions {
            for (key, value) in extensions {
                let value = async_graphql::Value::from_json(value)
                    .map_err(|e| bad_request(format!("invalid `extensions`: {e}")))?;
                request.extensions.insert(key, value);
            }
        }
        let incoming = Incoming {
            request,
            document_id: self.document_id.filter(|id| !id.is_empty()),
        };
        ensure_has_document(&incoming)?;
        Ok(incoming)
    }
}

/// A request must carry a document, a `documentId`, or an APQ hash.
fn ensure_has_document(incoming: &Incoming) -> Result<(), Rejection> {
    if incoming.request.query.trim().is_empty()
        && incoming.document_id.is_none()
        && !incoming.request.extensions.contains_key("persistedQuery")
    {
        return Err(bad_request("missing `query`"));
    }
    Ok(())
}

/// Parse a JSON body into one operation or a batch.
fn parse_json_body(body: &[u8]) -> Result<(Vec<Incoming>, bool), Rejection> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| bad_request(format!("request body is not valid JSON: {e}")))?;
    let parse = |value: serde_json::Value| -> Result<Incoming, Rejection> {
        serde_json::from_value::<WireRequest>(value)
            .map_err(|e| bad_request(format!("invalid GraphQL request: {e}")))?
            .into_incoming()
    };
    match value {
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return Err(bad_request("a batch must contain at least one operation"));
            }
            let parsed = items
                .into_iter()
                .map(parse)
                .collect::<Result<Vec<_>, _>>()?;
            Ok((parsed, true))
        }
        object @ serde_json::Value::Object(_) => Ok((vec![parse(object)?], false)),
        _ => Err(bad_request("request body must be a JSON object or array")),
    }
}

/// The GraphQL-over-HTTP `GET` parameters. `variables` and `extensions`
/// arrive as JSON text.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetParams {
    #[serde(default)]
    query: Option<String>,
    #[serde(default, alias = "operation_name")]
    operation_name: Option<String>,
    #[serde(default)]
    variables: Option<String>,
    #[serde(default)]
    extensions: Option<String>,
    #[serde(default)]
    document_id: Option<String>,
}

fn parse_get(uri_query: Option<&str>) -> Result<Incoming, Rejection> {
    let params: GetParams = serde_urlencoded_like(uri_query.unwrap_or(""))?;
    let json = |raw: Option<String>, what: &str| -> Result<Option<serde_json::Value>, Rejection> {
        raw.filter(|r| !r.trim().is_empty())
            .map(|r| {
                serde_json::from_str(&r).map_err(|e| bad_request(format!("invalid `{what}`: {e}")))
            })
            .transpose()
    };
    let extensions = match json(params.extensions, "extensions")? {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(map)) => Some(map),
        Some(_) => return Err(bad_request("`extensions` must be an object")),
    };
    WireRequest {
        query: params.query,
        operation_name: params.operation_name,
        variables: json(params.variables, "variables")?,
        extensions,
        document_id: params.document_id,
    }
    .into_incoming()
}

/// Decode `application/x-www-form-urlencoded` query parameters via axum's
/// own `Query` extractor machinery (`serde_urlencoded` underneath).
fn serde_urlencoded_like<T: serde::de::DeserializeOwned>(query: &str) -> Result<T, Rejection> {
    let uri: ::http::Uri = format!("/?{query}")
        .parse()
        .map_err(|_| bad_request("malformed query string"))?;
    axum::extract::Query::<T>::try_from_uri(&uri)
        .map(|q| q.0)
        .map_err(|e| bad_request(format!("invalid query string: {}", e.body_text())))
}

fn accept_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
}

fn not_acceptable() -> Response {
    rejection_response(
        Rejection::new(
            codes::NOT_ACCEPTABLE,
            "supported response types: application/graphql-response+json, application/json",
            StatusCode::NOT_ACCEPTABLE,
        ),
        Format::Json,
    )
}

/// `POST {path}`.
///
/// One linear dispatch over content types; splitting it would scatter the
/// order of checks the GraphQL-over-HTTP spec cares about.
#[allow(clippy::too_many_lines)]
pub async fn post_graphql<E: Executor>(
    State(state): State<AppState>,
    Extension(endpoint): Extension<Arc<Endpoint<E>>>,
    request: axum::extract::Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let Some(format) = negotiate(accept_header(&parts.headers), endpoint.sse) else {
        return not_acceptable();
    };
    let reply_format = if format == Format::EventStream {
        Format::Json
    } else {
        format
    };

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let multipart = mime == "multipart/form-data";
    let limit = if multipart {
        endpoint.max_body_bytes.saturating_add(
            endpoint
                .uploads
                .max_files
                .saturating_mul(endpoint.uploads.max_file_bytes),
        )
    } else {
        endpoint.max_body_bytes
    };

    let parsed = match mime.as_str() {
        "application/json" | "application/graphql-response+json" | "" => {
            match read_body(body, limit).await {
                Ok(bytes) => parse_json_body(&bytes),
                Err(response) => return *response,
            }
        }
        "application/graphql" => match read_body(body, limit).await {
            Ok(bytes) => String::from_utf8(bytes.to_vec())
                .map_err(|_| bad_request("body is not UTF-8"))
                .and_then(|query| {
                    let incoming = Incoming {
                        request: Request::new(query),
                        document_id: None,
                    };
                    ensure_has_document(&incoming).map(|()| (vec![incoming], false))
                }),
            Err(response) => return *response,
        },
        "multipart/form-data" if endpoint.uploads.enabled => {
            if endpoint.csrf_prevention && !has_preflight_header(&parts.headers) {
                return rejection_response(
                    Rejection::new(
                        codes::CSRF_PREVENTION,
                        "multipart requests must carry an `apollo-require-preflight` (or \
                         `graphql-require-preflight`) header",
                        StatusCode::BAD_REQUEST,
                    ),
                    reply_format,
                );
            }
            match read_body(body, limit).await {
                Ok(bytes) => parse_multipart(&content_type, bytes, &endpoint.uploads).await,
                Err(response) => return *response,
            }
        }
        _ => {
            return rejection_response(
                Rejection::new(
                    codes::UNSUPPORTED_MEDIA_TYPE,
                    format!("unsupported content type `{mime}`; send application/json"),
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ),
                reply_format,
            );
        }
    };

    let (operations, is_batch) = match parsed {
        Ok(parsed) => parsed,
        Err(rejection) => return rejection_response(rejection, reply_format),
    };

    if is_batch {
        if !endpoint.batching.enabled {
            return rejection_response(
                Rejection::new(
                    codes::BATCH_REJECTED,
                    "batched requests are not enabled on this endpoint",
                    StatusCode::BAD_REQUEST,
                ),
                reply_format,
            );
        }
        if operations.len() > endpoint.batching.max_operations {
            return rejection_response(
                Rejection::new(
                    codes::BATCH_REJECTED,
                    format!(
                        "a batch may carry at most {} operations (got {})",
                        endpoint.batching.max_operations,
                        operations.len()
                    ),
                    StatusCode::BAD_REQUEST,
                ),
                reply_format,
            );
        }
        return execute_batch(&endpoint, &state, &mut parts, operations, reply_format).await;
    }

    let Some(incoming) = operations.into_iter().next() else {
        return plain(StatusCode::BAD_REQUEST, "empty request");
    };
    if format == Format::EventStream {
        return super::sse::serve(endpoint, state, parts, incoming).await;
    }
    execute_single(
        &endpoint,
        &state,
        &mut parts,
        incoming,
        reply_format,
        Transport::HttpPost,
    )
    .await
}

/// `GET {path}?query=…` — queries only.
pub async fn get_graphql<E: Executor>(
    State(state): State<AppState>,
    Extension(endpoint): Extension<Arc<Endpoint<E>>>,
    mut parts: Parts,
) -> Response {
    let Some(format) = negotiate(accept_header(&parts.headers), endpoint.sse) else {
        return not_acceptable();
    };
    let incoming = match parse_get(parts.uri.query()) {
        Ok(incoming) => incoming,
        Err(rejection) => {
            let reply = if format == Format::EventStream {
                Format::Json
            } else {
                format
            };
            return rejection_response(rejection, reply);
        }
    };
    if format == Format::EventStream {
        return super::sse::serve(endpoint, state, parts, incoming).await;
    }
    if !endpoint.allow_get {
        return rejection_response(
            Rejection::new(
                codes::METHOD_NOT_ALLOWED,
                "GET is disabled on this endpoint; use POST",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            format,
        );
    }
    execute_single(
        &endpoint,
        &state,
        &mut parts,
        incoming,
        format,
        Transport::HttpGet,
    )
    .await
}

/// `GET {path}/sdl` — the schema as `text/plain` SDL.
pub async fn sdl<E: Executor>(Extension(endpoint): Extension<Arc<Endpoint<E>>>) -> Response {
    endpoint.sdl.as_ref().map_or_else(
        || plain(StatusCode::NOT_FOUND, "not found"),
        |sdl| {
            (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                sdl.to_string(),
            )
                .into_response()
        },
    )
}

async fn execute_single<E: Executor>(
    endpoint: &Endpoint<E>,
    state: &AppState,
    parts: &mut Parts,
    incoming: Incoming,
    format: Format,
    transport: Transport,
) -> Response {
    let Incoming {
        mut request,
        document_id,
    } = incoming;
    if let Err(rejection) = endpoint.attach(&mut request, parts, state, transport).await {
        return rejection_response(rejection, format);
    }
    let executed = endpoint
        .pipeline
        .execute_one(
            &endpoint.executor,
            request,
            document_id.as_deref(),
            transport,
        )
        .await;
    render_single(executed, format, parts.method == Method::GET)
}

async fn execute_batch<E: Executor>(
    endpoint: &Endpoint<E>,
    state: &AppState,
    parts: &mut Parts,
    operations: Vec<Incoming>,
    format: Format,
) -> Response {
    // Sequential, in order: a batch may mix mutations whose effects later
    // operations read, and concurrent execution would make that a race.
    let mut responses = Vec::with_capacity(operations.len());
    let mut headers = HeaderMap::new();
    for Incoming {
        mut request,
        document_id,
    } in operations
    {
        let response = match endpoint
            .attach(&mut request, parts, state, Transport::HttpPost)
            .await
        {
            Err(rejection) => return rejection_response(rejection, format),
            Ok(()) => {
                endpoint
                    .pipeline
                    .execute_one(
                        &endpoint.executor,
                        request,
                        document_id.as_deref(),
                        Transport::HttpPost,
                    )
                    .await
                    .response
            }
        };
        headers.extend(response.http_headers.clone());
        responses.push(response);
    }
    let body = serde_json::to_vec(&responses).unwrap_or_default();
    let mut response = json_response(StatusCode::OK, format, body);
    merge_headers(response.headers_mut(), headers);
    response
}

fn render_single(executed: Executed, format: Format, is_get: bool) -> Response {
    let Executed {
        response,
        status,
        transport_status,
        ..
    } = executed;
    let status = transport_status.unwrap_or(if format == Format::GraphqlResponseJson {
        status
    } else {
        StatusCode::OK
    });
    let cache_control = (is_get && response.errors.is_empty())
        .then(|| response.cache_control.value())
        .flatten();
    let resolver_headers = response.http_headers.clone();
    let body = serde_json::to_vec(&response).unwrap_or_else(|_| {
        serde_json::to_vec(&GqlResponse::from_errors(vec![])).unwrap_or_default()
    });
    let mut http_response = json_response(status, format, body);
    let headers = http_response.headers_mut();
    merge_headers(headers, resolver_headers);
    if is_get {
        headers.insert(header::VARY, HeaderValue::from_static("Accept"));
        if let Some(value) = cache_control.and_then(|v| HeaderValue::from_str(&v).ok()) {
            headers.insert(header::CACHE_CONTROL, value);
        }
    }
    http_response
}

/// Copy resolver-set headers (`ctx.insert_http_header`) onto the response,
/// never letting them change the negotiated content type.
fn merge_headers(target: &mut HeaderMap, extra: HeaderMap) {
    let mut last = None;
    for (name, value) in extra {
        let name = name.or_else(|| last.clone());
        let Some(name) = name else { continue };
        last = Some(name.clone());
        if name == header::CONTENT_TYPE || name == header::CONTENT_LENGTH {
            continue;
        }
        target.append(name, value);
    }
}

fn has_preflight_header(headers: &HeaderMap) -> bool {
    PREFLIGHT_HEADERS.iter().any(|name| {
        headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.trim().is_empty())
    })
}

/// Read the whole body, bounded by `limit`. The error is the finished HTTP
/// response, boxed to keep the `Result` small.
async fn read_body(
    body: axum::body::Body,
    limit: usize,
) -> Result<axum::body::Bytes, Box<Response>> {
    axum::body::to_bytes(body, limit).await.map_err(|error| {
        let too_large = std::error::Error::source(&error)
            .is_some_and(<dyn std::error::Error + 'static>::is::<http_body_util::LengthLimitError>)
            || error.to_string().contains("length limit");
        Box::new(if too_large {
            rejection_response(
                Rejection::new(
                    codes::PAYLOAD_TOO_LARGE,
                    format!("request body exceeds {limit} bytes"),
                    StatusCode::PAYLOAD_TOO_LARGE,
                ),
                Format::Json,
            )
        } else {
            plain(StatusCode::BAD_REQUEST, "could not read the request body")
        })
    })
}

async fn parse_multipart(
    content_type: &str,
    bytes: axum::body::Bytes,
    uploads: &crate::config::UploadsConfig,
) -> Result<(Vec<Incoming>, bool), Rejection> {
    let options = MultipartOptions::default()
        .max_file_size(uploads.max_file_bytes)
        .max_num_files(uploads.max_files);
    let batch = async_graphql::http::receive_batch_body(
        Some(content_type),
        futures_util::io::Cursor::new(bytes),
        options,
    )
    .await
    .map_err(|e| bad_request(format!("invalid multipart request: {e}")))?;
    let (requests, is_batch) = match batch {
        async_graphql::BatchRequest::Single(request) => (vec![request], false),
        async_graphql::BatchRequest::Batch(requests) => (requests, true),
    };
    let incoming = requests
        .into_iter()
        .map(|request| {
            let incoming = Incoming {
                request,
                document_id: None,
            };
            ensure_has_document(&incoming).map(|()| incoming)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((incoming, is_batch))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn json_bodies_parse_single_and_batch() {
        let (ops, batch) = parse_json_body(
            br#"{"query":"{ a }","operationName":"A","variables":{"x":1},"extensions":{"e":true}}"#,
        )
        .unwrap();
        assert!(!batch);
        assert_eq!(ops[0].request.query, "{ a }");
        assert_eq!(ops[0].request.operation_name.as_deref(), Some("A"));
        assert!(ops[0].request.extensions.contains_key("e"));

        let (ops, batch) = parse_json_body(br#"[{"query":"{ a }"},{"query":"{ b }"}]"#).unwrap();
        assert!(batch);
        assert_eq!(ops.len(), 2);

        let (ops, _) = parse_json_body(br#"{"documentId":"sha256:abc"}"#).unwrap();
        assert_eq!(ops[0].document_id.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn malformed_json_bodies_are_transport_errors() {
        for body in [
            &b"not json"[..],
            b"42",
            b"[]",
            br#"{"query":"{ a }","variables":[1]}"#,
            br#"{"query":7}"#,
            br#"{"variables":{}}"#,
        ] {
            let rejection = parse_json_body(body).err().unwrap();
            assert_eq!(rejection.status, StatusCode::BAD_REQUEST);
            assert!(rejection.transport_level);
        }
    }

    #[test]
    fn get_parameters_parse() {
        let incoming = parse_get(Some(
            "query=%7B%20a%20%7D&operationName=A&variables=%7B%22x%22%3A1%7D",
        ))
        .unwrap();
        assert_eq!(incoming.request.query, "{ a }");
        assert_eq!(incoming.request.operation_name.as_deref(), Some("A"));
        assert!(parse_get(None).is_err(), "missing query");
        assert!(parse_get(Some("query=%7Ba%7D&variables=nope")).is_err());
        assert!(parse_get(Some("query=%7Ba%7D&extensions=%5B%5D")).is_err());
        let apq = parse_get(Some(
            "extensions=%7B%22persistedQuery%22%3A%7B%22version%22%3A1%7D%7D",
        ))
        .unwrap();
        assert!(apq.request.query.is_empty());
    }

    #[test]
    fn preflight_headers_are_detected() {
        let mut headers = HeaderMap::new();
        assert!(!has_preflight_header(&headers));
        headers.insert("apollo-require-preflight", HeaderValue::from_static("true"));
        assert!(has_preflight_header(&headers));
    }

    #[test]
    fn resolver_headers_never_override_content_type() {
        let mut target = HeaderMap::new();
        target.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let mut extra = HeaderMap::new();
        extra.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        extra.append("set-cookie", HeaderValue::from_static("a=1"));
        extra.append("set-cookie", HeaderValue::from_static("b=2"));
        merge_headers(&mut target, extra);
        assert_eq!(target[header::CONTENT_TYPE], "application/json");
        assert_eq!(target.get_all("set-cookie").iter().count(), 2);
    }
}
