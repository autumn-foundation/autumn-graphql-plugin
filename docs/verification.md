# Verification record

How this crate was verified before its first release (2026-09-27), and
re-verified on the move to `autumn-web` 0.8 (2026-10-05).

## `autumn-web` 0.8.0 (2026-10-05)

| Build | Result |
|---|---|
| `autumn-web` 0.8.0 (crates.io), `--features test-support` | 94 tests pass (41 unit, 25 HTTP, 12 streaming, 5 persisted, 5 property, 5 conformance, 1 doc) |
| same, `--features boxed-trait,test-support` | pass |
| `cargo +1.88.0 check` (MSRV) | pass |
| `cargo clippy --all-targets -D warnings` (pedantic + nursery), both feature sets | clean |
| `cargo doc -D warnings` | clean |
| `cargo run --example notes` + `curl` | query, authenticated mutation, coded `UNAUTHENTICATED`, SDL, `graphql_*` metrics |

The contract is now always declared; the conformance suite runs it through
Autumn's startup gate and `run_conformance`.

## First release, against `autumn-web` 0.7.0 (2026-09-27)

### This crate's suite

| Build | Result |
|---|---|
| `autumn-web` 0.7.0 (crates.io), `--features test-support` | 92 tests pass (41 unit, 25 HTTP, 12 streaming, 5 persisted, 5 property, 3 conformance, 1 doc) |
| same, `--features boxed-trait,test-support` | pass |
| Autumn `main` via `[patch.crates-io]`, `--features test-support,plugin-contract` | pass (adds the contract test) |
| `cargo +1.88.0 check` (MSRV) | pass |
| `cargo clippy --all-targets -D warnings` (pedantic + nursery), both feature sets | clean |
| `cargo llvm-cov` | 94.3 % lines |

Property tests run 512 generated documents per property.

### Drop-in replacement for the react-graphql example

In a local checkout of `autumn-foundation/autumn` (`main`), the example's
in-tree plugin was replaced by this crate:

```rust
// examples/react-graphql/src/graphql_plugin.rs
pub use autumn_plugin_graphql::{GraphqlPlugin, PLUGIN_NAME};
```

```rust
// examples/react-graphql/src/lib.rs — the one signature that changes
pub fn graphql()
-> GraphqlPlugin<async_graphql::Schema<notes::Query, notes::Mutation, async_graphql::EmptySubscription>> {
```

```toml
# examples/react-graphql/Cargo.toml
autumn-plugin-graphql = { path = "…", features = ["plugin-contract"] }
```

`cargo test -p react-graphql --test graphql_api`: **10 passed, 0 failed**,
with the example's tests unchanged — SDL route, `405` for mutations over
`GET`, two plugins at two paths, the `RequireApiToken` guard, CSRF under
`prod`, `400` for `GET` without a query, redacted missing-pool errors, the
committed-SDL drift gate, and plugin conformance with the contract. The 8
Docker (testcontainers) tests were not run: no Docker daemon was available.

### Live smoke test

`cargo run --example notes`, exercised with `curl`: query; `UNAUTHENTICATED`
without a bearer token; authenticated mutation; a subscription over SSE
receiving the event produced by a concurrent mutation; coded
`VALIDATION_FAILED`; APQ miss → register → hit over `GET`; a depth violation
as `400` under `application/graphql-response+json`; SDL; and the
`graphql_*` families at `/actuator/prometheus`.
