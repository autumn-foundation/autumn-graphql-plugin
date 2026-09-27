# ADR 0005 — `[graphql]` configuration with Autumn's layering

- Status: accepted
- Date: 2026-09-27

## Context

Operators need to change GraphQL behaviour per environment (introspection
off in production, trusted documents only in production) and in incidents
(a kill switch) without a code change. Autumn core does not hand plugins
their config section; `autumn-search` resolves its own section with the full
layering to avoid profile overrides being silently ignored.

## Decision

- Declare `[graphql]` with `config_section` (strict-config safe) and resolve
  it like core: base file, inline `[profile.X.graphql]`, `autumn-X.toml`,
  then `AUTUMN_GRAPHQL__*` (with `.env`), using core's own helpers for profile
  names and file lookup.
- Environment overrides are derived generically from the section's leaf keys,
  so new settings are overridable without extra code.
- Fluent setters in code apply on top of the resolved section;
  `.config(GraphqlConfig)` bypasses files entirely.
- Three-state `Toggle` (`true`/`false`/`"auto"`) for settings whose safe
  default depends on context (profile, presence of a subscription root).
- A malformed file value refuses boot; a malformed environment value is
  logged and ignored (matching core).

## Consequences

- One `[graphql]` section serves every endpoint unless an endpoint names its
  own with `.config_section(..)`.
- The resolver duplicates a small amount of core's layering logic; it uses
  core's public helpers wherever they exist.
