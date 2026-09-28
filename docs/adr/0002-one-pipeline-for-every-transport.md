# ADR 0002 — One pipeline for every transport

- Status: accepted
- Date: 2026-09-27

## Context

GraphQL servers commonly harden their HTTP handler and forget the WebSocket:
depth limits, persisted-query rules or introspection policy that apply to
`POST /graphql` do not apply to `subscribe` messages. async-graphql's
WebSocket implementation calls `Executor::execute_stream` directly.

## Decision

All checks live in `pipeline::Pipeline::prepare`. HTTP calls it via
`execute_one`. WebSocket and SSE hand async-graphql a `GuardedExecutor<E>`
that implements `Executor` by calling `prepare` before delegating. The
pipeline parses each document once through `Request::parsed_query`, which
caches the parsed document on the request; async-graphql's executor takes that
cached document instead of parsing again, so analysis adds no second parse.

## Consequences

- A new transport cannot forget a check: it has no other way to reach the
  executor.
- async-graphql's `boxed-trait` feature changes the `Executor` trait's shape,
  so the crate mirrors it with its own `boxed-trait` feature.
- Timeouts apply to request/response operations only; streams are bounded by
  keep-alive, connection caps and draining instead.
