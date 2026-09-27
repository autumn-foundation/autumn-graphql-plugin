//! Keep a committed SDL file in step with the live schema.
//!
//! Frontends generate types from a committed `schema.graphql`. This check,
//! run in a test, fails when the file drifts from what the server serves —
//! and regenerates it when `AUTUMN_GRAPHQL_BLESS=1` is set:
//!
//! ```rust,ignore
//! #[test]
//! fn committed_schema_matches_the_live_sdl() {
//!     autumn_plugin_graphql::sdl::assert_committed_sdl(
//!         &notes::build_schema().sdl(),
//!         concat!(env!("CARGO_MANIFEST_DIR"), "/schema.graphql"),
//!     );
//! }
//! ```

use std::path::Path;

/// Environment variable that makes the check rewrite the file instead of
/// failing.
pub const BLESS_ENV: &str = "AUTUMN_GRAPHQL_BLESS";

/// The committed file differs from the live SDL.
#[derive(Debug, thiserror::Error)]
pub enum SdlDrift {
    /// The file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// The contents differ.
    #[error(
        "{path} is stale — rerun with {BLESS_ENV}=1 to regenerate it\n--- first difference at line {line} ---\ncommitted: {committed}\nlive:      {live}"
    )]
    Stale {
        /// The file.
        path: String,
        /// 1-based line of the first difference.
        line: usize,
        /// That line in the file.
        committed: String,
        /// That line in the live SDL.
        live: String,
    },
}

/// Compare `live` with the file at `path` (ignoring line endings and
/// surrounding whitespace). With `AUTUMN_GRAPHQL_BLESS=1`, write `live` to
/// the file instead.
///
/// # Errors
///
/// [`SdlDrift::Stale`] on a mismatch, [`SdlDrift::Io`] if the file cannot be
/// read (or, when blessing, written).
pub fn check_committed_sdl(live: &str, path: impl AsRef<Path>) -> Result<(), SdlDrift> {
    let path = path.as_ref();
    let io = |source| SdlDrift::Io {
        path: path.display().to_string(),
        source,
    };
    if std::env::var(BLESS_ENV).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return std::fs::write(path, format!("{}\n", live.trim())).map_err(io);
    }
    let committed = std::fs::read_to_string(path).map_err(io)?;
    let committed = committed.replace("\r\n", "\n");
    let (committed, live) = (committed.trim(), live.trim());
    if committed == live {
        return Ok(());
    }
    let mut left = committed.lines();
    let mut right = live.lines();
    let mut line_number = 1;
    loop {
        match (left.next(), right.next()) {
            (Some(a), Some(b)) if a == b => line_number += 1,
            (a, b) => {
                return Err(SdlDrift::Stale {
                    path: path.display().to_string(),
                    line: line_number,
                    committed: a.unwrap_or("<end of file>").to_owned(),
                    live: b.unwrap_or("<end of schema>").to_owned(),
                });
            }
        }
    }
}

/// [`check_committed_sdl`], panicking with the drift report.
///
/// # Panics
///
/// When the committed file is stale or unreadable.
#[allow(clippy::panic)]
pub fn assert_committed_sdl(live: &str, path: impl AsRef<Path>) {
    if let Err(drift) = check_committed_sdl(live, path) {
        panic!("{drift}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn detects_drift_and_reports_the_first_differing_line() {
        let dir = std::env::temp_dir().join(format!("autumn-graphql-sdl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("schema.graphql");
        std::fs::write(&file, "type Query {\r\n  a: Int\r\n}\r\n").unwrap();
        check_committed_sdl("type Query {\n  a: Int\n}", &file).unwrap();

        let drift = check_committed_sdl("type Query {\n  b: Int\n}", &file).unwrap_err();
        let SdlDrift::Stale {
            line,
            committed,
            live,
            ..
        } = &drift
        else {
            panic!("expected stale, got {drift}");
        };
        assert_eq!(
            (*line, committed.as_str(), live.as_str()),
            (2, "  a: Int", "  b: Int")
        );
        assert!(drift.to_string().contains(BLESS_ENV));

        let drift = check_committed_sdl("type Query {\n  a: Int\n}\ntype X", &file).unwrap_err();
        assert!(matches!(drift, SdlDrift::Stale { line: 4, .. }));
        assert!(check_committed_sdl("x", dir.join("missing.graphql")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
