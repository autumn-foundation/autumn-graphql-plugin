//! The plugin-owned `[graphql]` section of `autumn.toml`.
//!
//! Every knob has a production-safe default, so a zero-config mount is already
//! hardened. The section is resolved with the same layering Autumn core uses
//! for its own configuration:
//!
//! 1. base `autumn.toml` `[graphql]`,
//! 2. inline `[profile.<name>.graphql]` (legacy alias first, canonical last),
//! 3. `autumn-<profile>.toml` `[graphql]` (first existing file wins),
//! 4. `AUTUMN_GRAPHQL__*` environment variables (including `.env` files).
//!
//! Reading only layer 1 would make `introspection = false` under
//! `[profile.prod.graphql]` a silent no-op in production — exactly where it is
//! needed — so the plugin pays for the full merge.
//!
//! ```toml
//! [graphql]
//! path = "/graphql"
//! introspection = "auto"          # auto = on in dev/test, off elsewhere
//! mask_unexpected_errors = "auto" # auto = off in dev/test, on elsewhere
//! timeout_ms = 30000
//!
//! [graphql.limits]
//! max_depth = 15
//! max_aliases = 30
//!
//! [graphql.persisted_queries]
//! mode = "trusted"                # only allow-listed operations run
//! manifest = "persisted-queries.json"
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use autumn_web::config::Env;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Environment-variable prefix for `[graphql]` overrides.
pub const ENV_PREFIX: &str = "AUTUMN_GRAPHQL__";

/// The default config root this plugin reads.
pub const DEFAULT_SECTION: &str = "graphql";

/// A three-state switch: forced on, forced off, or decided by context
/// (the active profile, or whether the schema has a subscription root).
///
/// In TOML it is written as a boolean or the string `"auto"`:
///
/// ```toml
/// introspection = false
/// sdl = "auto"
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Toggle {
    /// Decided by context; see each field's documentation.
    #[default]
    Auto,
    /// Always on.
    On,
    /// Always off.
    Off,
}

impl Toggle {
    /// Resolve against the context-derived default.
    #[must_use]
    pub const fn resolve(self, auto: bool) -> bool {
        match self {
            Self::Auto => auto,
            Self::On => true,
            Self::Off => false,
        }
    }
}

impl From<bool> for Toggle {
    fn from(value: bool) -> Self {
        if value { Self::On } else { Self::Off }
    }
}

impl Serialize for Toggle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::On => serializer.serialize_bool(true),
            Self::Off => serializer.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for Toggle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bool(value) => Ok(value.into()),
            Raw::Text(text) => match text.trim().to_ascii_lowercase().as_str() {
                "auto" => Ok(Self::Auto),
                "true" | "on" | "enabled" | "yes" => Ok(Self::On),
                "false" | "off" | "disabled" | "no" => Ok(Self::Off),
                other => Err(serde::de::Error::custom(format!(
                    "expected true, false or \"auto\", found \"{other}\""
                ))),
            },
        }
    }
}

/// How persisted operations are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedQueryMode {
    /// Free-form documents only. A hash-only request is refused with
    /// `PERSISTED_QUERY_NOT_SUPPORTED` so the client falls back to sending
    /// the full document.
    #[default]
    Disabled,
    /// Apollo Automatic Persisted Queries: a client may send only a
    /// `sha256Hash`; on a miss it re-sends the document, which is verified
    /// and cached. Free-form documents are still accepted. Trusted documents
    /// from a manifest are served too.
    Automatic,
    /// Trusted documents (a safelist): only operations whose hash is in the
    /// manifest may run, whether the client sends the hash or the full text.
    /// Everything else is refused with `OPERATION_NOT_ALLOWLISTED`.
    Trusted,
}

/// Request batching (`[ {..}, {..} ]` bodies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct BatchingConfig {
    /// Accept JSON-array batch bodies. Off by default: a batch multiplies the
    /// work one HTTP request can demand.
    pub enabled: bool,
    /// The most operations one batch may carry.
    pub max_operations: usize,
}

impl Default for BatchingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_operations: 10,
        }
    }
}

/// Static analysis limits enforced on every document before it executes.
///
/// `0` means "unlimited" for every field. The defaults admit any realistic
/// application query while refusing the pathological shapes used to exhaust
/// a GraphQL server (deep nesting, alias amplification, fragment fan-out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct LimitsConfig {
    /// Largest accepted document, in bytes.
    pub max_query_bytes: usize,
    /// Deepest lexical nesting of `{`, `(` and `[`, checked before parsing so
    /// the parser itself is never handed a stack-exhausting input.
    pub max_nesting: usize,
    /// Deepest selection-set nesting of the executed operation, with
    /// fragments expanded. `{ a { b } }` has depth 2.
    pub max_depth: usize,
    /// Most aliased fields in the executed operation, with fragments expanded.
    pub max_aliases: usize,
    /// Most fields selected directly on the operation's root type.
    pub max_root_fields: usize,
    /// Most field selections in the executed operation after fragment
    /// expansion — the guard against fragment fan-out ("fragment bombs").
    pub max_fields: usize,
    /// Most directives in the document.
    pub max_directives: usize,
    /// Schema-level complexity limit, applied when the plugin builds the
    /// schema itself (`GraphqlPlugin::from_builder`). Uses async-graphql's
    /// `#[graphql(complexity = ..)]` cost model. `0` disables it.
    pub max_complexity: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_query_bytes: 32 * 1024,
            max_nesting: 64,
            max_depth: 15,
            max_aliases: 30,
            max_root_fields: 30,
            max_fields: 1_000,
            max_directives: 50,
            max_complexity: 0,
        }
    }
}

/// Subscriptions over WebSocket and Server-Sent Events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct SubscriptionsConfig {
    /// Mount `GET {path}/ws` (`graphql-transport-ws` and legacy `graphql-ws`).
    /// `auto` = on when the schema has a subscription root.
    pub websocket: Toggle,
    /// Serve operations as `text/event-stream` when the client asks for it
    /// (GraphQL over SSE, "distinct connections" mode). `auto` = on when the
    /// schema has a subscription root.
    pub sse: Toggle,
    /// Most concurrent WebSocket + SSE streams per endpoint; `0` = unlimited.
    pub max_connections: usize,
    /// Keep-alive interval, in seconds, for WebSocket pings and SSE comments.
    pub keepalive_secs: u64,
    /// Seconds a WebSocket client has to send `connection_init` before the
    /// server closes it with `4408`.
    pub init_timeout_secs: u64,
}

impl Default for SubscriptionsConfig {
    fn default() -> Self {
        Self {
            websocket: Toggle::Auto,
            sse: Toggle::Auto,
            max_connections: 10_000,
            keepalive_secs: 30,
            init_timeout_secs: 10,
        }
    }
}

/// Persisted operations: Automatic Persisted Queries and trusted documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct PersistedQueriesConfig {
    /// See [`PersistedQueryMode`].
    pub mode: PersistedQueryMode,
    /// Capacity of the in-memory APQ cache (least recently used evicted).
    pub cache_capacity: usize,
    /// Path to a trusted-document manifest (Apollo
    /// `persisted-query-manifest.json`, or a flat `{ "<sha256>": "<doc>" }`
    /// map). Empty = none. Relative paths resolve like `autumn.toml`.
    pub manifest: String,
}

impl Default for PersistedQueriesConfig {
    fn default() -> Self {
        Self {
            mode: PersistedQueryMode::Disabled,
            cache_capacity: 1_000,
            manifest: String::new(),
        }
    }
}

/// `multipart/form-data` uploads (the GraphQL multipart request spec).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct UploadsConfig {
    /// Accept multipart requests. Off by default.
    pub enabled: bool,
    /// Most files per request.
    pub max_files: usize,
    /// Largest single file, in bytes.
    pub max_file_bytes: usize,
}

impl Default for UploadsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_files: 10,
            max_file_bytes: 10 * 1024 * 1024,
        }
    }
}

/// The `[graphql]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct GraphqlConfig {
    /// Kill switch. `false` mounts nothing (the routes 404).
    pub enabled: bool,
    /// Mount path of the endpoint.
    pub path: String,
    /// Allow introspection (`__schema`, `__type`). `auto` = on in the `dev`
    /// and `test` profiles, off elsewhere. `__typename` is always allowed.
    pub introspection: Toggle,
    /// Serve `GET {path}/sdl`. `auto` follows the effective `introspection`
    /// value: publishing the SDL is publishing the schema.
    pub sdl: Toggle,
    /// Replace the message of any resolver error that does not carry an
    /// `extensions.code` with `Internal server error` (the original is
    /// logged). `auto` = off in `dev`/`test`, on elsewhere.
    pub mask_unexpected_errors: Toggle,
    /// Serve queries over `GET {path}?query=…`. Mutations over `GET` are
    /// always refused with `405`.
    pub allow_get: bool,
    /// Largest accepted request body, in bytes (multipart uploads are
    /// bounded separately by `[graphql.uploads]`).
    pub max_body_bytes: usize,
    /// Per-operation execution timeout in milliseconds; `0` = none.
    /// Subscriptions are not timed out.
    pub timeout_ms: u64,
    /// Operations slower than this (milliseconds) are logged at `WARN`;
    /// `0` = never.
    pub slow_operation_ms: u64,
    /// Require a preflight-forcing header (`apollo-require-preflight`,
    /// `graphql-require-preflight` or `x-apollo-operation-name`) on
    /// multipart requests, which browsers otherwise send cross-site without
    /// a CORS preflight.
    pub csrf_prevention: bool,
    /// See [`BatchingConfig`].
    pub batching: BatchingConfig,
    /// See [`LimitsConfig`].
    pub limits: LimitsConfig,
    /// See [`SubscriptionsConfig`].
    pub subscriptions: SubscriptionsConfig,
    /// See [`PersistedQueriesConfig`].
    pub persisted_queries: PersistedQueriesConfig,
    /// See [`UploadsConfig`].
    pub uploads: UploadsConfig,
}

impl Default for GraphqlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/graphql".to_owned(),
            introspection: Toggle::Auto,
            sdl: Toggle::Auto,
            mask_unexpected_errors: Toggle::Auto,
            allow_get: true,
            max_body_bytes: 1024 * 1024,
            timeout_ms: 30_000,
            slow_operation_ms: 1_000,
            csrf_prevention: true,
            batching: BatchingConfig::default(),
            limits: LimitsConfig::default(),
            subscriptions: SubscriptionsConfig::default(),
            persisted_queries: PersistedQueriesConfig::default(),
            uploads: UploadsConfig::default(),
        }
    }
}

/// A configuration problem. Boot is refused rather than falling back to
/// defaults: a typo'd `introspection = flase` must not silently leave
/// introspection on in production.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid [graphql] configuration: {0}")]
pub struct ConfigError(pub String);

impl GraphqlConfig {
    /// Parse the `[section]` table out of a whole `autumn.toml` document,
    /// with no profile layering and no environment overrides.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for invalid TOML, unknown keys, wrong types,
    /// or values [`validate`](Self::validate) rejects.
    pub fn from_toml_str(text: &str, section: &str) -> Result<Self, ConfigError> {
        let document: toml::Table = toml::from_str(text).map_err(|e| ConfigError(e.to_string()))?;
        let config = Self::from_section(document.get(section))?;
        config.validate()?;
        Ok(config)
    }

    fn from_section(section: Option<&toml::Value>) -> Result<Self, ConfigError> {
        section.map_or_else(
            || Ok(Self::default()),
            |value| {
                value
                    .clone()
                    .try_into()
                    .map_err(|e: toml::de::Error| ConfigError(e.to_string()))
            },
        )
    }

    /// Resolve `[section]` from the host app's configuration files and
    /// environment, exactly as Autumn core layers its own settings.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a contributing file cannot be read or
    /// parsed, when the merged section has an unknown or ill-typed key, or
    /// when a value is out of range.
    pub fn resolve(section: &str) -> Result<Resolved, ConfigError> {
        autumn_web::dotenv::os_env_with_dotenv().map_or_else(
            |_| Self::resolve_with_env(section, &autumn_web::config::OsEnv),
            |env| Self::resolve_with_env(section, &env),
        )
    }

    /// Pure core of [`resolve`](Self::resolve), reading only `env`.
    ///
    /// # Errors
    ///
    /// See [`resolve`](Self::resolve).
    pub fn resolve_with_env(section: &str, env: &dyn Env) -> Result<Resolved, ConfigError> {
        let (selected, canonical) = resolve_active_profile(env);
        let mut merged = toml::Value::Table(toml::map::Map::new());

        let base = read_optional_toml(&find_config_file("autumn.toml", env))?;
        if let Some(base) = &base {
            deep_merge(&mut merged, base.clone());
            for name in profile_inline_lookup_names(&canonical) {
                if let Some(profile) = profile_section(base, name) {
                    deep_merge(&mut merged, profile);
                }
            }
        }
        for name in autumn_web::config::profile_override_file_lookup_names(&canonical, &selected) {
            let path = find_config_file(&format!("autumn-{name}.toml"), env);
            if let Some(overlay) = read_optional_toml(&path)? {
                deep_merge(&mut merged, overlay);
                break;
            }
        }

        let mut section_value = merged
            .get(section)
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        if !section_value.is_table() {
            return Err(ConfigError(format!("`{section}` must be a table")));
        }
        let mut config = Self::from_section(Some(&section_value))?;
        apply_env_overrides(&mut section_value, &mut config, env);
        config.validate()?;
        Ok(Resolved {
            config,
            profile: canonical,
            base_dir: config_base_dir(env),
        })
    }

    /// Reject values that would misbehave at runtime.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] naming the offending key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let path = self.path.trim();
        if !path.starts_with('/') || path.len() < 2 || path.ends_with('/') {
            return Err(ConfigError(format!(
                "graphql.path must start with `/`, must not end with `/`, and must not be the \
                 root (got {:?})",
                self.path
            )));
        }
        if path.contains(['{', '}', '*', '?', '#', ' ']) {
            return Err(ConfigError(format!(
                "graphql.path must be a literal path (got {:?})",
                self.path
            )));
        }
        if self.max_body_bytes == 0 {
            return Err(ConfigError(
                "graphql.max_body_bytes must be at least 1".into(),
            ));
        }
        if self.batching.enabled && self.batching.max_operations == 0 {
            return Err(ConfigError(
                "graphql.batching.max_operations must be at least 1 when batching is enabled"
                    .into(),
            ));
        }
        if self.uploads.enabled && (self.uploads.max_files == 0 || self.uploads.max_file_bytes == 0)
        {
            return Err(ConfigError(
                "graphql.uploads.max_files and max_file_bytes must be at least 1 when uploads \
                 are enabled"
                    .into(),
            ));
        }
        if self.persisted_queries.mode == PersistedQueryMode::Automatic
            && self.persisted_queries.cache_capacity == 0
        {
            return Err(ConfigError(
                "graphql.persisted_queries.cache_capacity must be at least 1 in automatic mode"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// A resolved configuration plus the context it was resolved in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The merged, validated section.
    pub config: GraphqlConfig,
    /// The canonical active profile (`dev`, `prod`, `test`, …).
    pub profile: String,
    /// Directory relative manifest paths resolve against.
    pub base_dir: PathBuf,
}

impl Resolved {
    /// A resolution for configuration supplied in code: no files are read,
    /// but the profile still comes from the environment.
    #[must_use]
    pub fn explicit(config: GraphqlConfig) -> Self {
        let env = autumn_web::dotenv::os_env_with_dotenv().ok();
        let (profile, base_dir) = env.as_ref().map_or_else(
            || {
                let os = autumn_web::config::OsEnv;
                (resolve_active_profile(&os).1, config_base_dir(&os))
            },
            |env| (resolve_active_profile(env).1, config_base_dir(env)),
        );
        Self {
            config,
            profile,
            base_dir,
        }
    }

    /// `dev` and `test` get developer-friendly `auto` defaults
    /// (introspection on, errors unmasked); every other profile is treated
    /// as production.
    #[must_use]
    pub fn is_development(&self) -> bool {
        matches!(self.profile.as_str(), "dev" | "test")
    }
}

/// Apply `AUTUMN_GRAPHQL__<PATH>` overrides for every leaf key the section
/// knows, e.g. `AUTUMN_GRAPHQL__LIMITS__MAX_DEPTH=10`.
///
/// Values are read as TOML literals (`true`, `10`, `"x"`), falling back to a
/// bare string. An override that does not type-check is logged and ignored,
/// matching core: a malformed environment variable must not take down a
/// process that would boot on its file configuration.
fn apply_env_overrides(section: &mut toml::Value, config: &mut GraphqlConfig, env: &dyn Env) {
    let Ok(defaults) = toml::Value::try_from(GraphqlConfig::default()) else {
        return;
    };
    let mut leaves = Vec::new();
    collect_leaves(&defaults, &mut Vec::new(), &mut leaves);
    for path in leaves {
        let key = format!(
            "{ENV_PREFIX}{}",
            path.iter()
                .map(|segment| segment.to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join("__")
        );
        let Some(raw) = env_trimmed(env, &key) else {
            continue;
        };
        let value = parse_env_value(&raw);
        let mut candidate = section.clone();
        set_path(&mut candidate, &path, value);
        match GraphqlConfig::from_section(Some(&candidate)) {
            Ok(parsed) => {
                *section = candidate;
                *config = parsed;
            }
            Err(error) => tracing::warn!(
                variable = %key,
                %error,
                "ignoring an [graphql] environment override that does not type-check"
            ),
        }
    }
}

fn parse_env_value(raw: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut table| table.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_owned()))
}

fn collect_leaves(value: &toml::Value, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    if let toml::Value::Table(table) = value {
        for (key, child) in table {
            prefix.push(key.clone());
            collect_leaves(child, prefix, out);
            prefix.pop();
        }
    } else {
        out.push(prefix.clone());
    }
}

fn set_path(root: &mut toml::Value, path: &[String], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cursor = root;
    for segment in parents {
        let Some(table) = cursor.as_table_mut() else {
            return;
        };
        cursor = table
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    if let Some(table) = cursor.as_table_mut() {
        table.insert(last.clone(), value);
    }
}

/// Non-blank env value (a blank value reads as unset, as core treats it).
fn env_trimmed(env: &dyn Env, key: &str) -> Option<String> {
    env.var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The `(selected spelling, canonical name)` of the active profile.
///
/// Mirrors core's resolution: `AUTUMN_ENV` → `AUTUMN_PROFILE` → `--profile`
/// → `AUTUMN_IS_DEBUG=0` ⇒ `prod` → `dev`.
fn resolve_active_profile(env: &dyn Env) -> (String, String) {
    let selected = resolve_profile_input(env);
    let canonical =
        autumn_web::config::normalize_profile_name(&selected).unwrap_or_else(|| "dev".to_owned());
    (selected, canonical)
}

fn resolve_profile_input(env: &dyn Env) -> String {
    if let Some(value) = env_trimmed(env, "AUTUMN_ENV") {
        return value;
    }
    if let Some(value) = env_trimmed(env, "AUTUMN_PROFILE") {
        return value;
    }
    let args: Vec<String> = std::env::args().collect();
    for (index, arg) in args.iter().enumerate() {
        if arg == "--profile"
            && let Some(profile) = args.get(index.saturating_add(1))
            && !profile.trim().is_empty()
        {
            return profile.trim().to_owned();
        }
        if let Some(profile) = arg.strip_prefix("--profile=")
            && !profile.trim().is_empty()
        {
            return profile.trim().to_owned();
        }
    }
    if env_trimmed(env, "AUTUMN_IS_DEBUG").as_deref() == Some("0") {
        return "prod".to_owned();
    }
    "dev".to_owned()
}

/// Resolve one config filename the way core does: `AUTUMN_MANIFEST_DIR` when
/// the file is there, else relative to the working directory.
fn find_config_file(filename: &str, env: &dyn Env) -> PathBuf {
    if let Some(dir) = env_trimmed(env, "AUTUMN_MANIFEST_DIR") {
        let candidate = PathBuf::from(dir).join(filename);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(filename)
}

/// Where relative paths inside the section (the manifest) resolve from: the
/// directory holding `autumn.toml`.
fn config_base_dir(env: &dyn Env) -> PathBuf {
    find_config_file("autumn.toml", env)
        .parent()
        .map(Path::to_path_buf)
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_optional_toml(path: &Path) -> Result<Option<toml::Value>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let table = toml::from_str::<toml::Table>(&contents)
                .map_err(|e| ConfigError(format!("cannot parse {}: {e}", path.display())))?;
            Ok(Some(toml::Value::Table(table)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

fn profile_inline_lookup_names(canonical: &str) -> Vec<&str> {
    match canonical {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

fn profile_section(base: &toml::Value, profile: &str) -> Option<toml::Value> {
    base.get("profile")
        .and_then(toml::Value::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(toml::Value::as_table)
        .map(|table| toml::Value::Table(table.clone()))
}

/// Deep-merge `overlay` into `base`: tables merge recursively, everything
/// else replaces. Bounded like core's own merge.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    deep_merge_at(base, overlay, 0);
}

fn deep_merge_at(base: &mut toml::Value, overlay: toml::Value, depth: usize) {
    const MAX_MERGE_DEPTH: usize = 16;
    if depth > MAX_MERGE_DEPTH {
        return;
    }
    let toml::Value::Table(overlay_table) = overlay else {
        return;
    };
    let Some(base_table) = base.as_table_mut() else {
        return;
    };
    for (key, overlay_value) in overlay_table {
        let recurse =
            overlay_value.is_table() && base_table.get(&key).is_some_and(toml::Value::is_table);
        if recurse {
            if let Some(base_value) = base_table.get_mut(&key) {
                deep_merge_at(base_value, overlay_value, depth.saturating_add(1));
            }
        } else {
            base_table.insert(key, overlay_value);
        }
    }
}

impl fmt::Display for Toggle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::On => "true",
            Self::Off => "false",
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use autumn_web::config::MockEnv;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "autumn-graphql-config-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_are_production_safe() {
        let config = GraphqlConfig::default();
        assert!(config.enabled);
        assert_eq!(config.path, "/graphql");
        assert_eq!(config.introspection, Toggle::Auto);
        assert!(!config.batching.enabled);
        assert!(!config.uploads.enabled);
        assert!(config.csrf_prevention);
        assert_eq!(config.limits.max_depth, 15);
        config.validate().unwrap();
    }

    #[test]
    fn a_missing_section_is_the_defaults() {
        assert_eq!(
            GraphqlConfig::from_toml_str("[server]\nport = 3000\n", "graphql").unwrap(),
            GraphqlConfig::default()
        );
        assert_eq!(
            GraphqlConfig::from_toml_str("", "graphql").unwrap(),
            GraphqlConfig::default()
        );
    }

    #[test]
    fn a_partial_section_keeps_the_other_defaults() {
        let config = GraphqlConfig::from_toml_str(
            "[graphql]\nintrospection = false\n[graphql.limits]\nmax_depth = 7\n",
            "graphql",
        )
        .unwrap();
        assert_eq!(config.introspection, Toggle::Off);
        assert_eq!(config.limits.max_depth, 7);
        assert_eq!(config.limits.max_aliases, 30);
        assert_eq!(config.path, "/graphql");
    }

    #[test]
    fn typos_and_wrong_types_are_errors_not_defaults() {
        assert!(
            GraphqlConfig::from_toml_str("[graphql]\nintrospectoin = false\n", "graphql").is_err()
        );
        assert!(
            GraphqlConfig::from_toml_str("[graphql]\nintrospection = \"flase\"\n", "graphql")
                .is_err()
        );
        assert!(
            GraphqlConfig::from_toml_str("[graphql.limits]\nmax_depth = \"x\"\n", "graphql")
                .is_err()
        );
        assert!(GraphqlConfig::from_toml_str("[graphql\n", "graphql").is_err());
    }

    #[test]
    fn toggles_accept_booleans_and_words() {
        for (text, expected) in [
            ("true", Toggle::On),
            ("false", Toggle::Off),
            ("\"auto\"", Toggle::Auto),
            ("\"on\"", Toggle::On),
            ("\"disabled\"", Toggle::Off),
        ] {
            let config =
                GraphqlConfig::from_toml_str(&format!("[graphql]\nsdl = {text}\n"), "graphql")
                    .unwrap();
            assert_eq!(config.sdl, expected, "{text}");
        }
        assert!(Toggle::Auto.resolve(true));
        assert!(!Toggle::Auto.resolve(false));
        assert!(Toggle::On.resolve(false));
        assert!(!Toggle::Off.resolve(true));
    }

    #[test]
    fn invalid_values_are_rejected() {
        for bad in [
            "[graphql]\npath = \"graphql\"\n",
            "[graphql]\npath = \"/\"\n",
            "[graphql]\npath = \"/graphql/\"\n",
            "[graphql]\npath = \"/{id}\"\n",
            "[graphql]\nmax_body_bytes = 0\n",
            "[graphql.batching]\nenabled = true\nmax_operations = 0\n",
            "[graphql.uploads]\nenabled = true\nmax_files = 0\n",
            "[graphql.persisted_queries]\nmode = \"automatic\"\ncache_capacity = 0\n",
            "[graphql.persisted_queries]\nmode = \"sometimes\"\n",
        ] {
            assert!(
                GraphqlConfig::from_toml_str(bad, "graphql").is_err(),
                "should reject: {bad}"
            );
        }
    }

    #[test]
    fn profile_layers_and_env_override_in_order() {
        let dir = temp_dir("layers");
        std::fs::write(
            dir.join("autumn.toml"),
            "[graphql]\npath = \"/api/graphql\"\nintrospection = true\ntimeout_ms = 5\n\
             [graphql.limits]\nmax_depth = 20\n\
             [profile.prod.graphql]\nintrospection = false\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("autumn-prod.toml"),
            "[graphql.limits]\nmax_depth = 12\n",
        )
        .unwrap();

        let env = MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", dir.to_str().unwrap())
            .with("AUTUMN_ENV", "prod")
            .with("AUTUMN_GRAPHQL__TIMEOUT_MS", "250")
            .with("AUTUMN_GRAPHQL__LIMITS__MAX_ALIASES", "3")
            .with("AUTUMN_GRAPHQL__PERSISTED_QUERIES__MODE", "trusted");
        let resolved = GraphqlConfig::resolve_with_env("graphql", &env).unwrap();
        assert_eq!(resolved.profile, "prod");
        assert!(!resolved.is_development());
        let config = resolved.config;
        assert_eq!(config.path, "/api/graphql", "base layer");
        assert_eq!(config.introspection, Toggle::Off, "inline profile layer");
        assert_eq!(config.limits.max_depth, 12, "profile file layer");
        assert_eq!(config.timeout_ms, 250, "env layer");
        assert_eq!(config.limits.max_aliases, 3, "nested env layer");
        assert_eq!(config.persisted_queries.mode, PersistedQueryMode::Trusted);
        assert_eq!(resolved.base_dir, dir);

        let dev = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.to_str().unwrap());
        let resolved = GraphqlConfig::resolve_with_env("graphql", &dev).unwrap();
        assert_eq!(resolved.profile, "dev");
        assert!(resolved.is_development());
        assert_eq!(resolved.config.introspection, Toggle::On);
        assert_eq!(resolved.config.limits.max_depth, 20);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_malformed_env_override_is_ignored() {
        let env = MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", "/nonexistent-autumn-graphql")
            .with("AUTUMN_GRAPHQL__LIMITS__MAX_DEPTH", "deep")
            .with("AUTUMN_GRAPHQL__INTROSPECTION", "auto")
            .with("AUTUMN_GRAPHQL__ENABLED", "false");
        let config = GraphqlConfig::resolve_with_env("graphql", &env)
            .unwrap()
            .config;
        assert_eq!(config.limits.max_depth, 15);
        assert_eq!(config.introspection, Toggle::Auto);
        assert!(!config.enabled);
    }

    #[test]
    fn a_custom_section_name_is_read() {
        let config = GraphqlConfig::from_toml_str(
            "[graphql]\npath = \"/a\"\n[graphql_admin]\npath = \"/admin/graphql\"\n",
            "graphql_admin",
        )
        .unwrap();
        assert_eq!(config.path, "/admin/graphql");
    }

    #[test]
    fn an_unreadable_file_is_an_error() {
        let dir = temp_dir("unparseable");
        std::fs::write(dir.join("autumn.toml"), "[graphql\n").unwrap();
        let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.to_str().unwrap());
        assert!(GraphqlConfig::resolve_with_env("graphql", &env).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
