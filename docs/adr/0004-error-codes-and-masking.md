# ADR 0004 — Stable error codes and masking of unexpected errors

- Status: accepted
- Date: 2026-09-27

## Context

A GraphQL response is an HTTP `200` under `application/json`, so it bypasses
Autumn's problem-details filter — the place where server-side error detail is
redacted. async-graphql serialises any resolver error's `Display` into
`errors[].message`, including `?` on database errors.

## Decision

- Every error leaving the plugin carries `extensions.code` (a documented
  vocabulary in `error::codes`), and pre-execution refusals carry
  `extensions.status`.
- `AutumnError` converts through `IntoGraphqlError`/`.gql()`: `4xx` keep their
  message, `5xx` are logged and replaced; validation details become both the
  message and `extensions.fields`. The conversion reads core's own
  `AutumnErrorInfo` (stashed by `into_response`), so it honours core's status
  remapping and works on the published `autumn-web` 0.7.0, which has no public
  `details()` accessor.
- An execution error without a code is "unexpected". With
  `mask_unexpected_errors` (default: on outside `dev`/`test`) its message is
  replaced and the original logged.

## Consequences

- Apps opt errors *into* visibility by giving them a code — the same model as
  GraphQL Yoga's masked errors.
- Introspection refusals are explicit (`INTROSPECTION_DISABLED`) rather than
  async-graphql's silent `null`, which clients misread as an empty schema.
