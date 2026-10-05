# ADR 0006 — Build against published autumn-web, gate `main`-only APIs

- Status: superseded by [ADR 0007](0007-autumn-web-0.8.md)
- Date: 2026-09-27

## Context

The react-graphql example targets Autumn's `main` branch, which has APIs the
published `autumn-web` 0.7.0 does not: `plugin_contract` / `Plugin::contract`,
`AppBuilder::plugin_route_infos`, `AutumnError::details`. A crate on
crates.io must build against published releases.

## Decision

- Depend on `autumn-web = "0.7"` from crates.io, `default-features = false`.
- Put `Plugin::contract` behind a `plugin-contract` feature, to be made
  default once a release ships it.
- Avoid `details()` by reading `AutumnErrorInfo` (present in both).
- CI runs the suite against crates.io 0.7 and, with `plugin-contract`,
  against Autumn `main` via a `[patch.crates-io]` override.

## Consequences

- `cargo test --all-features` fails against 0.7.0 by design; CI lists
  feature sets explicitly.
