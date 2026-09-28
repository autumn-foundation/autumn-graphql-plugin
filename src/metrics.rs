//! Metrics, published through Autumn's own registry so they appear at
//! `/actuator/prometheus` next to the framework's `autumn_http_*` families.
//!
//! | Family | Kind | Labels |
//! |---|---|---|
//! | `graphql_operations_total` | counter | `endpoint`, `transport`, `operation` (`query`/`mutation`/`subscription`/`unknown`), `outcome` (`success`/`error`/`rejected`/`started`) |
//! | `graphql_operation_duration_seconds` | histogram | `endpoint`, `transport`, `operation` |
//! | `graphql_errors_total` | counter | `endpoint`, `code` |
//! | `graphql_active_streams` | gauge | `endpoint`, `transport` |
//!
//! The `autumn_` prefix is reserved by the framework for its own families
//! (the registry refuses it), hence `graphql_`.
//!
//! Label values are drawn from closed sets (operation *names* are
//! deliberately not a label — they are client-controlled and unbounded).
//! `code` is bounded in practice by the codes your resolvers use.

use std::sync::Arc;
use std::time::Duration;

use async_graphql::parser::types::OperationType;

use crate::context::Transport;
use crate::pipeline::kind_label;

/// Per-endpoint metric recorder.
#[derive(Clone, Debug)]
pub struct Metrics {
    endpoint: Arc<str>,
}

impl Metrics {
    pub(crate) fn new(endpoint: &str) -> Self {
        static DESCRIBED: std::sync::Once = std::sync::Once::new();
        DESCRIBED.call_once(|| {
            autumn_web::metrics::describe_counter(
                "graphql_operations_total",
                "GraphQL operations by endpoint, transport, operation type and outcome",
            );
            autumn_web::metrics::describe_histogram(
                "graphql_operation_duration_seconds",
                "GraphQL operation latency, from request preparation to response",
            );
            autumn_web::metrics::describe_counter(
                "graphql_errors_total",
                "GraphQL errors by endpoint and extensions.code",
            );
            autumn_web::metrics::describe_gauge(
                "graphql_active_streams",
                "Open GraphQL WebSocket connections and SSE streams",
            );
        });
        Self {
            endpoint: Arc::from(endpoint),
        }
    }

    pub(crate) fn operation(
        &self,
        transport: Transport,
        kind: Option<OperationType>,
        outcome: &'static str,
        elapsed: Option<Duration>,
    ) {
        autumn_web::metrics::counter("graphql_operations_total")
            .with_label("endpoint", &*self.endpoint)
            .with_label("transport", transport.as_str())
            .with_label("operation", kind_label(kind))
            .with_label("outcome", outcome)
            .increment(1u64);
        if let Some(elapsed) = elapsed {
            autumn_web::metrics::histogram("graphql_operation_duration_seconds")
                .with_label("endpoint", &*self.endpoint)
                .with_label("transport", transport.as_str())
                .with_label("operation", kind_label(kind))
                .record(elapsed.as_secs_f64());
        }
    }

    pub(crate) fn error(&self, code: &str) {
        autumn_web::metrics::counter("graphql_errors_total")
            .with_label("endpoint", &*self.endpoint)
            .with_label("code", code)
            .increment(1u64);
    }

    pub(crate) fn stream_opened(&self, transport: Transport) {
        autumn_web::metrics::gauge("graphql_active_streams")
            .with_label("endpoint", &*self.endpoint)
            .with_label("transport", transport.as_str())
            .increment(1u64);
    }

    pub(crate) fn stream_closed(&self, transport: Transport) {
        autumn_web::metrics::gauge("graphql_active_streams")
            .with_label("endpoint", &*self.endpoint)
            .with_label("transport", transport.as_str())
            .decrement(1u64);
    }
}
