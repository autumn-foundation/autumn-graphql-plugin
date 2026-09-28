# ADR 0001 — A standalone plugin crate, generic over the executor

- Status: accepted
- Date: 2026-09-27

## Context

Autumn's `examples/react-graphql` contained a ~350-line `GraphqlPlugin` that
adapted an async-graphql `Schema<Q, M, S>` onto an app: `POST`/`GET`, an SDL
route, a guard seam, declared routes and a contract. Its README noted that
lifting it into a published crate would be "a copy of one file plus a
`Cargo.toml`". Production use needs much more (limits, persisted queries,
subscriptions, error hygiene, metrics, configuration).

## Decision

- Publish it as `autumn-plugin-graphql` with `GraphqlPlugin` at the root —
  Autumn's third-party naming convention (`autumn-plugin-<name>` /
  `<Name>Plugin`), so `autumn plugin add` can find it.
- Make the plugin generic over `E: async_graphql::Executor` instead of
  `<Q, M, S>`. `GraphqlPlugin::new(Schema<Q, M, S>)` keeps the example's
  one-liner; `from_executor` admits `dynamic::Schema` and any custom executor;
  `from_builder` lets the plugin install schema-level limits.
- Keep the example's builder surface (`new`, `path`, `guard`, `without_sdl`,
  `route_infos`) source-compatible.
- Keep the example's mounting model: a nested router carrying its state as an
  `axum::Extension`, plus `declare_plugin_routes`.

## Consequences

- The type parameter changes from `<Q, M, S>` to `<Schema<Q, M, S>>`; one
  signature in an app changes when migrating.
- Autumn refuses to boot an app with zero typed routes, and nested routers do
  not count. A GraphQL-only service must register one typed route. We chose
  not to hand-construct `autumn_web::Route` values (documented as
  macro-generated, and its field set grows between releases).
