# ADR 0003 — Plugin-level static analysis limits

- Status: accepted
- Date: 2026-09-27

## Context

async-graphql offers `limit_depth`, `limit_complexity`, etc., but only on a
`SchemaBuilder`: an app that hands the plugin a finished `Schema` cannot be
hardened by the plugin, and operators cannot tune builder calls from
configuration. The parser itself is recursive, so a sufficiently nested
document is a risk before any schema-level check runs. Fragment spreads make
naive "count the fields" exponential.

## Decision

- Check size and lexical nesting on the raw text before parsing.
- Analyse the executed operation (fragments expanded) for depth, aliases,
  root fields, total fields, directives and introspection, with fragment
  results memoized so analysis is linear; detect cycles; bound recursion.
- Make every limit configurable (`0` = unlimited) with defaults that admit
  realistic application queries.
- Additionally apply async-graphql's schema-level limits when the plugin
  builds the schema (`from_builder`), including its complexity model.

## Consequences

- Limits apply uniformly across transports and to persisted documents.
- The analyzer is attacker-facing, so its invariants are stated in the module
  docs and property-tested against a naive expander.
- The analyzer's notion of cost is structural; resolver-aware cost needs
  `max_complexity` with `from_builder`.
