# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the crate uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `GraphqlPlugin`: mount any async-graphql `Schema`, `SchemaBuilder` or
  `Executor` on an Autumn app; builder surface compatible with the
  `examples/react-graphql` plugin it grew out of.
- GraphQL over HTTP: `POST` (JSON, `application/graphql`), `GET` (queries
  only; `405` for mutations), `application/graphql-response+json` status
  semantics, `406`/`415` negotiation, opt-in batching, opt-in multipart
  uploads with CSRF preflight, `Cache-Control` from `@cacheControl` on `GET`,
  resolver-set headers.
- Subscriptions over WebSocket (`graphql-transport-ws`, legacy `graphql-ws`)
  and SSE, through the same pipeline as HTTP; init timeout (`4408`), refused
  init (`4403`), connection caps, keep-alive, draining (`1001`) on shutdown
  or via `DrainHandle`.
- Static analysis limits: size, lexical nesting (pre-parse), depth, aliases,
  root fields, fields (fragment-bomb safe), directives; async-graphql
  complexity via `from_builder`; explicit `INTROSPECTION_DISABLED`.
- Persisted operations: Apollo APQ with a pluggable `PersistedQueryStore`
  (in-memory LRU default); trusted documents from Apollo manifests or flat
  maps; `documentId` requests.
- Errors: stable `extensions.code` vocabulary; `IntoGraphqlError` /
  `GraphqlResultExt::gql` for `AutumnError` with `5xx` redaction and
  validation `fields`; masking of uncoded resolver errors.
- Context: `AppState` and `GraphqlRequestInfo` in every resolver;
  `ContextHook`s over request parts; `WsInitHook` for `connection_init`.
- `[graphql]` configuration with profile layering, `AUTUMN_GRAPHQL__*`
  overrides, `Toggle` auto settings, kill switch, validation that refuses
  boot on bad values.
- Metrics (`graphql_operations_total`, `graphql_operation_duration_seconds`,
  `graphql_errors_total`, `graphql_active_streams`), `graphql.operation`
  tracing spans, slow-operation log.
- `sdl::assert_committed_sdl` drift check; `testing::GraphqlTestExt`
  (feature `test-support`).
- `Plugin::contract` declaring `autumn-web` `0.8` (`SUPPORTED_AUTUMN_WEB`),
  checked by `autumn plugin-check` and Autumn's startup gate.
- Feature: `boxed-trait`.

### Changed

- Built against `autumn-web` 0.8 (was 0.7). The contract is declared
  unconditionally; the `plugin-contract` feature is gone (ADR 0007).
