# CLAUDE.md — autumn-plugin-graphql

Production-grade GraphQL plugin for the Autumn web framework, built on
async-graphql 7. One crate, published as `autumn-plugin-graphql`.

## Commands

- Format: `cargo fmt --all` (CI: `cargo fmt --all --check`)
- Lint: `cargo clippy --all-targets --features test-support -- -D warnings`
  (pedantic + nursery are on in `Cargo.toml`)
- Test: `cargo test --features test-support`
- Test the boxed-trait build: `cargo test --features boxed-trait,test-support`
- Test against Autumn `main` (+ contract):
  `cargo test --features test-support,plugin-contract --config 'patch.crates-io.autumn-web.git="https://github.com/autumn-foundation/autumn"'`
- Example: `cargo run --example notes`

**Do not run `--all-features` against crates.io autumn-web**: the
`plugin-contract` feature needs Autumn `main` (see ADR 0006).

## Layout

See `docs/architecture.md`. In short: `plugin.rs` (builder + `Plugin` impl),
`config.rs` (`[graphql]`), `pipeline.rs` (the only road to the executor),
`limits.rs` (analyzer), `persisted.rs`, `error.rs`, `context.rs`,
`transport/{http,ws,sse}.rs`, `metrics.rs`, `sdl.rs`, `testing.rs`.

## Rules

- **Every transport goes through `Pipeline::prepare`.** New transports use
  `execute_one` or `GuardedExecutor`; never call the executor directly.
- **Every error leaving the plugin has `extensions.code`.** New refusal
  kinds get a constant in `error::codes` and a row in the README.
- **The analyzer is attacker-facing.** Keep it total (saturating arithmetic,
  bounded recursion) and keep `tests/limits_properties.rs` in step with any
  change to what it measures.
- **Config:** new settings go in `config.rs` with a production-safe default,
  a doc comment, validation if a value can misbehave, and a line in the
  README's TOML block. Env overrides come for free (leaf-key derivation).
- **No `unwrap`/`expect`/`panic!` in library code**; tests may use them.
- **Metric names must not start with `autumn_`** — the framework registry
  rejects that namespace silently.
- No behaviour without a test. HTTP behaviour → `tests/http.rs`; streams →
  `tests/streaming.rs`; persisted → `tests/persisted.rs`; framework fit →
  `tests/conformance.rs`.
- Record significant decisions as `docs/adr/NNNN-*.md`.

## Autumn API notes (0.7.0)

- `AppBuilder::run` panics when no typed routes are registered; nested
  routers do not count.
- `AppBuilder::with_extension` stores on the builder only; values reach
  `AppState` through `state_initializer` / `AppState::insert_extension`.
- `TestApp` runs plugin startup hooks but not shutdown hooks; test draining
  through `GraphqlPlugin::drain_handle`.
