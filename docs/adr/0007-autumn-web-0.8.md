# ADR 0007 — Target autumn-web 0.8; declare the plugin contract unconditionally

- Status: accepted
- Date: 2026-10-05
- Supersedes: [ADR 0006](0006-autumn-version-compatibility.md)

## Context

ADR 0006 built against `autumn-web` 0.7.0 and put `Plugin::contract` behind
a `plugin-contract` feature until a release shipped `autumn_web::plugin_contract`.
`autumn-web` 0.8.0 is that release: it has `plugin_contract`,
`AppBuilder::plugin_route_infos` and `AutumnError::details`.

0.8 also *enforces* contracts. `AppBuilder::plugin` evaluates the declared
`autumn-web` range against the linked framework and panics on a mismatch
(unless `AUTUMN_PLUGIN_CONTRACT=warn`). A plugin whose contract says `0.7`
cannot be mounted in a 0.8 app.

## Decision

- Depend on `autumn-web = "0.8"` (crates.io, `default-features = false`).
  0.7 is no longer supported; the two series are not semver-compatible.
- Declare `Plugin::contract` unconditionally with
  `SUPPORTED_AUTUMN_WEB = "0.8"`. Remove the `plugin-contract` feature (the
  crate was unreleased, so nobody depends on it).
- Test the contract directly: it must evaluate `Compatible` against the
  linked `AUTUMN_WEB_VERSION` and `Incompatible` against other series, and
  the conformance tests read the real builder manifest through
  `plugin_route_infos` instead of reconstructing it.
- Keep reading `AutumnErrorInfo` for error conversion (ADR 0004): it is
  still present and honours core's status remapping; switching to
  `details()` buys nothing.
- CI keeps a non-blocking job against Autumn `main`. If `main` moves to a
  new series the contract gate will refuse the plugin there — that failure
  is the signal to cut a release for the next series.

## Consequences

- `cargo test --all-features` works again; no feature needs a git
  dependency.
- Each future Autumn minor series needs a release of this crate that bumps
  `SUPPORTED_AUTUMN_WEB` together with the dependency requirement.
