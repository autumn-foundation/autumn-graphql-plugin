//! Static analysis of GraphQL documents, enforced before execution.
//!
//! Two layers, cheapest first:
//!
//! 1. [`lexical_nesting`] scans the raw text for the deepest `{ ( [` nesting,
//!    skipping strings and comments. It runs before the parser, so an input
//!    built to exhaust the parser's stack never reaches it.
//! 2. [`analyze`] walks the *executed* operation of a parsed document with
//!    fragments expanded, and reports its depth, alias count, root-field
//!    count, total field count and directive count.
//!
//! # Invariants
//!
//! The analyzer is the part of this crate an attacker controls the input
//! to, so its contract is stated precisely and property-tested
//! (`tests/limits_properties.rs`):
//!
//! - **Totality.** For every document it returns `Ok` or `Err`; it never
//!   panics, never overflows (all arithmetic saturates) and never recurses
//!   deeper than [`MAX_ANALYSIS_RECURSION`].
//! - **Linear cost.** Each fragment definition is analysed at most once and
//!   memoized, so a "fragment bomb" (`F1` spreading `F0` twice, `F2`
//!   spreading `F1` twice, …) is `O(size)` to analyse even though its
//!   expansion is `O(2^n)`.
//! - **Faithfulness.** For an acyclic document the memoized result equals
//!   the result of naively expanding every spread (saturating at
//!   `usize::MAX`).
//! - **Cycle safety.** A fragment that (transitively) spreads itself is
//!   reported as [`AnalysisError::FragmentCycle`] — the spec forbids it and
//!   expanding it would not terminate.
//! - **Monotonicity.** A document is refused iff some measured statistic
//!   exceeds its non-zero limit; `0` means unlimited.

use std::collections::HashMap;

use async_graphql::Positioned;
use async_graphql::parser::types::{
    DocumentOperations, ExecutableDocument, FragmentDefinition, OperationDefinition, OperationType,
    Selection, SelectionSet,
};

use crate::config::LimitsConfig;

/// Deepest recursion the analyzer allows itself.
///
/// Counts selection sets plus fragment spreads along one path. Documents that need more are refused
/// as [`AnalysisError::TooComplex`]: at that point the document is
/// pathological whatever the configured limits say.
pub const MAX_ANALYSIS_RECURSION: usize = 512;

/// Statistics of one operation, with fragments expanded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OperationStats {
    /// Deepest selection-set nesting. `{ a { b } }` is 2; `{ __typename }` is 1.
    pub depth: usize,
    /// Aliased fields.
    pub aliases: usize,
    /// Fields selected directly on the root type (through fragments too).
    pub root_fields: usize,
    /// Field selections in total.
    pub fields: usize,
    /// Directive applications, including those on the operation itself.
    pub directives: usize,
    /// Whether the operation selects `__schema` or `__type` (introspection;
    /// `__typename` does not count).
    pub introspection: bool,
}

/// Why a document could not be analysed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnalysisError {
    /// A fragment spreads itself, directly or transitively.
    #[error("fragment `{0}` spreads itself")]
    FragmentCycle(String),
    /// The document nests deeper than the analyzer will follow.
    #[error("document is nested too deeply to analyse")]
    TooComplex,
    /// No operation matches the requested name (or the document has several
    /// and none was named). The executor reports these itself.
    #[error("no operation selected")]
    NoOperation,
}

/// A statistic over its configured limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitViolation {
    /// Which limit: `query_bytes`, `nesting`, `depth`, `aliases`,
    /// `root_fields`, `fields` or `directives`.
    pub limit: &'static str,
    /// The configured maximum.
    pub max: usize,
    /// What the document measured (for the analyzer's statistics, a
    /// saturating count).
    pub actual: usize,
}

impl std::fmt::Display for LimitViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.limit {
            "query_bytes" => "document size in bytes",
            "nesting" => "lexical nesting",
            "depth" => "query depth",
            "aliases" => "number of aliases",
            "root_fields" => "number of root fields",
            "fields" => "number of fields",
            "directives" => "number of directives",
            other => other,
        };
        write!(
            f,
            "{what} {} exceeds the limit of {}",
            self.actual, self.max
        )
    }
}

/// `actual` against `max`, where `max == 0` means unlimited.
const fn over(limit: &'static str, max: usize, actual: usize) -> Option<LimitViolation> {
    if max != 0 && actual > max {
        Some(LimitViolation { limit, max, actual })
    } else {
        None
    }
}

/// The checks that run on raw text, before parsing.
///
/// # Errors
///
/// Returns the first violated limit: size first, then nesting.
pub fn check_text(query: &str, limits: &LimitsConfig) -> Result<(), LimitViolation> {
    if let Some(v) = over("query_bytes", limits.max_query_bytes, query.len()) {
        return Err(v);
    }
    if limits.max_nesting != 0 {
        // Stop scanning one past the limit: the exact figure is not needed.
        let nesting = lexical_nesting(query, limits.max_nesting.saturating_add(1));
        if let Some(v) = over("nesting", limits.max_nesting, nesting) {
            return Err(v);
        }
    }
    Ok(())
}

/// Compare analysed statistics against the limits.
///
/// # Errors
///
/// Returns the first violated limit, in the order depth, aliases,
/// root fields, fields, directives.
pub fn check_stats(stats: &OperationStats, limits: &LimitsConfig) -> Result<(), LimitViolation> {
    [
        over("depth", limits.max_depth, stats.depth),
        over("aliases", limits.max_aliases, stats.aliases),
        over("root_fields", limits.max_root_fields, stats.root_fields),
        over("fields", limits.max_fields, stats.fields),
        over("directives", limits.max_directives, stats.directives),
    ]
    .into_iter()
    .flatten()
    .next()
    .map_or(Ok(()), Err)
}

/// Deepest nesting of `{`, `(` and `[` in `text`, ignoring string literals,
/// block strings and comments. Scanning stops as soon as `stop_at` is
/// reached (pass `usize::MAX` for the exact figure).
///
/// Unbalanced closers never drive the depth below zero.
#[must_use]
pub fn lexical_nesting(text: &str, stop_at: usize) -> usize {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut depth = 0usize;
    let mut max = 0usize;
    while i < len {
        match bytes[i] {
            b'#' => {
                while i < len && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
            }
            b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                i += 3;
                while i < len {
                    if bytes[i..].starts_with(b"\\\"\"\"") {
                        i += 4;
                    } else if bytes[i..].starts_with(b"\"\"\"") {
                        i += 3;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                i += 1;
                while i < len {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\n' | b'\r' => break,
                        _ => i += 1,
                    }
                }
            }
            b'{' | b'(' | b'[' => {
                depth = depth.saturating_add(1);
                if depth > max {
                    max = depth;
                    if max >= stop_at {
                        return max;
                    }
                }
                i += 1;
            }
            b'}' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    max
}

/// The operation a request will execute: the one named `operation_name`, or
/// the only one.
#[must_use]
pub fn select_operation<'a>(
    document: &'a ExecutableDocument,
    operation_name: Option<&str>,
) -> Option<&'a Positioned<OperationDefinition>> {
    match (&document.operations, operation_name) {
        (DocumentOperations::Single(op), _) => Some(op),
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name),
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => ops.values().next(),
        (DocumentOperations::Multiple(_), None) => None,
    }
}

/// The type (`query`/`mutation`/`subscription`) of the selected operation.
#[must_use]
pub fn operation_type(
    document: &ExecutableDocument,
    operation_name: Option<&str>,
) -> Option<OperationType> {
    select_operation(document, operation_name).map(|op| op.node.ty)
}

/// Analyse the operation a request will execute.
///
/// # Errors
///
/// See [`AnalysisError`].
pub fn analyze(
    document: &ExecutableDocument,
    operation_name: Option<&str>,
) -> Result<OperationStats, AnalysisError> {
    let operation = select_operation(document, operation_name).ok_or(AnalysisError::NoOperation)?;
    let mut analyzer = Analyzer {
        fragments: &document.fragments,
        memo: HashMap::new(),
    };
    let set = analyzer.selection_set(&operation.node.selection_set.node, 0)?;
    Ok(OperationStats {
        depth: set.depth,
        aliases: set.aliases,
        root_fields: set.flat_fields,
        fields: set.fields,
        directives: set
            .directives
            .saturating_add(operation.node.directives.len()),
        introspection: set.introspection,
    })
}

/// Statistics of one selection set. `flat_fields` counts the fields that
/// land on the set's own type (through spreads and inline fragments), which
/// is what `root_fields` means at the top level.
#[derive(Debug, Clone, Copy, Default)]
struct SetStats {
    depth: usize,
    aliases: usize,
    flat_fields: usize,
    fields: usize,
    directives: usize,
    introspection: bool,
}

impl SetStats {
    fn absorb(&mut self, other: Self) {
        self.introspection |= other.introspection;
        self.depth = self.depth.max(other.depth);
        self.aliases = self.aliases.saturating_add(other.aliases);
        self.flat_fields = self.flat_fields.saturating_add(other.flat_fields);
        self.fields = self.fields.saturating_add(other.fields);
        self.directives = self.directives.saturating_add(other.directives);
    }
}

enum Memo {
    InProgress,
    Done(SetStats),
}

struct Analyzer<'a> {
    fragments: &'a HashMap<async_graphql::Name, Positioned<FragmentDefinition>>,
    memo: HashMap<&'a str, Memo>,
}

impl<'a> Analyzer<'a> {
    fn selection_set(
        &mut self,
        set: &'a SelectionSet,
        recursion: usize,
    ) -> Result<SetStats, AnalysisError> {
        if recursion > MAX_ANALYSIS_RECURSION {
            return Err(AnalysisError::TooComplex);
        }
        let next = recursion + 1;
        let mut stats = SetStats::default();
        for item in &set.items {
            match &item.node {
                Selection::Field(field) => {
                    let field = &field.node;
                    let children = self.selection_set(&field.selection_set.node, next)?;
                    stats.absorb(SetStats {
                        depth: children.depth.saturating_add(1),
                        aliases: children
                            .aliases
                            .saturating_add(usize::from(field.alias.is_some())),
                        flat_fields: 1,
                        fields: children.fields.saturating_add(1),
                        directives: children.directives.saturating_add(field.directives.len()),
                        introspection: children.introspection
                            || matches!(field.name.node.as_str(), "__schema" | "__type"),
                    });
                }
                Selection::InlineFragment(inline) => {
                    let inline = &inline.node;
                    let mut inner = self.selection_set(&inline.selection_set.node, next)?;
                    inner.directives = inner.directives.saturating_add(inline.directives.len());
                    stats.absorb(inner);
                }
                Selection::FragmentSpread(spread) => {
                    let spread = &spread.node;
                    let mut inner = self.fragment(spread.fragment_name.node.as_str(), next)?;
                    inner.directives = inner.directives.saturating_add(spread.directives.len());
                    stats.absorb(inner);
                }
            }
        }
        Ok(stats)
    }

    fn fragment(&mut self, name: &'a str, recursion: usize) -> Result<SetStats, AnalysisError> {
        match self.memo.get(name) {
            Some(Memo::Done(stats)) => return Ok(*stats),
            Some(Memo::InProgress) => return Err(AnalysisError::FragmentCycle(name.to_owned())),
            None => {}
        }
        // An unknown fragment contributes nothing here; the executor's own
        // validation reports it with a proper message.
        let Some(definition) = self.fragments.get(name) else {
            return Ok(SetStats::default());
        };
        self.memo.insert(name, Memo::InProgress);
        let mut stats = self.selection_set(&definition.node.selection_set.node, recursion)?;
        stats.directives = stats
            .directives
            .saturating_add(definition.node.directives.len());
        self.memo.insert(name, Memo::Done(stats));
        Ok(stats)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fmt::Write as _;

    use async_graphql::parser::parse_query;

    use super::*;

    fn stats(query: &str) -> OperationStats {
        analyze(&parse_query(query).unwrap(), None).unwrap()
    }

    #[test]
    fn measures_depth_aliases_and_fields() {
        assert_eq!(
            stats("{ __typename }"),
            OperationStats {
                depth: 1,
                aliases: 0,
                root_fields: 1,
                fields: 1,
                directives: 0,
                introspection: false
            }
        );
        assert_eq!(
            stats("{ a: notes { id b: title } c: notes { id } }"),
            OperationStats {
                depth: 2,
                aliases: 3,
                root_fields: 2,
                fields: 5,
                directives: 0,
                introspection: false
            }
        );
        assert_eq!(
            stats("query Q @x { a @y { ... on T @z { b } } }"),
            OperationStats {
                depth: 2,
                aliases: 0,
                root_fields: 1,
                fields: 2,
                directives: 3,
                introspection: false
            }
        );
        assert!(stats("{ __schema { types { name } } }").introspection);
        assert!(
            stats("{ ...F } fragment F on Query { t: __type(name: \"X\") { name } }").introspection
        );
        assert!(!stats("{ __typename a { __typename } }").introspection);
    }

    #[test]
    fn expands_fragments_including_at_the_root() {
        let s = stats(
            "query { ...Root x { ...Leaf } } \
             fragment Root on Query { a b { ...Leaf } } \
             fragment Leaf on T { c d: e }",
        );
        // root fields: x, a, b; fields: x, c, e, a, b, c, e.
        assert_eq!(s.root_fields, 3);
        assert_eq!(s.fields, 7);
        assert_eq!(s.aliases, 2);
        assert_eq!(s.depth, 2);
    }

    #[test]
    fn fragment_bombs_are_linear_and_saturate() {
        let mut query = String::from("query { ...F0 }\nfragment F0 on Query { a b }\n");
        for i in 1..200 {
            let _ = writeln!(
                query,
                "fragment F{i} on Query {{ ...F{j} ...F{j} }}",
                j = i - 1
            );
        }
        query = query.replacen("...F0", "...F199", 1);
        let s = stats(&query);
        assert_eq!(s.fields, usize::MAX, "2^200 saturates");
        assert_eq!(s.depth, 1);
    }

    #[test]
    fn cycles_are_reported() {
        let doc = parse_query(
            "query { ...A } fragment A on Query { ...B } fragment B on Query { x { ...A } }",
        )
        .unwrap();
        assert_eq!(
            analyze(&doc, None),
            Err(AnalysisError::FragmentCycle("A".into()))
        );
        let doc = parse_query("query { ...A } fragment A on Query { ...A }").unwrap();
        assert!(matches!(
            analyze(&doc, None),
            Err(AnalysisError::FragmentCycle(_))
        ));
    }

    #[test]
    fn long_spread_chains_hit_the_recursion_guard() {
        let mut query = String::from("query { ...F0 }\n");
        for i in 0..2_000 {
            let _ = writeln!(query, "fragment F{i} on Query {{ ...F{} }}", i + 1);
        }
        query.push_str("fragment F2000 on Query { a }\n");
        let doc = parse_query(&query).unwrap();
        assert_eq!(analyze(&doc, None), Err(AnalysisError::TooComplex));
    }

    #[test]
    fn selects_the_named_operation() {
        let doc = parse_query("query A { a } mutation B { b { c } }").unwrap();
        assert_eq!(analyze(&doc, Some("B")).unwrap().depth, 2);
        assert_eq!(operation_type(&doc, Some("A")), Some(OperationType::Query));
        assert_eq!(
            operation_type(&doc, Some("B")),
            Some(OperationType::Mutation)
        );
        assert_eq!(analyze(&doc, None), Err(AnalysisError::NoOperation));
        assert_eq!(analyze(&doc, Some("C")), Err(AnalysisError::NoOperation));
        // A named operation must be selected by its own name, as the executor
        // requires; an anonymous one is selected whatever the name says.
        let named = parse_query("query A { a }").unwrap();
        assert_eq!(operation_type(&named, None), Some(OperationType::Query));
        assert_eq!(operation_type(&named, Some("other")), None);
        let anonymous = parse_query("{ a }").unwrap();
        assert_eq!(
            operation_type(&anonymous, Some("other")),
            Some(OperationType::Query)
        );
    }

    #[test]
    fn unknown_fragments_count_as_empty() {
        assert_eq!(stats("{ a ...Missing }").fields, 1);
    }

    #[test]
    fn nesting_ignores_strings_and_comments() {
        assert_eq!(lexical_nesting("{ a { b(x: [1]) } }", usize::MAX), 4);
        assert_eq!(lexical_nesting("{ a(s: \"{{{{\") }", usize::MAX), 2);
        assert_eq!(lexical_nesting("{ a(s: \"\\\"{{\") }", usize::MAX), 2);
        assert_eq!(
            lexical_nesting("{ a(s: \"\"\"{{{ \\\"\"\" {{\"\"\") }", usize::MAX),
            2
        );
        assert_eq!(lexical_nesting("# {{{{{\n{ a }", usize::MAX), 1);
        assert_eq!(lexical_nesting("}}}{", usize::MAX), 1);
        assert_eq!(lexical_nesting("{{{{{{{{", 3), 3, "stops early");
        assert_eq!(lexical_nesting("", usize::MAX), 0);
    }

    #[test]
    fn checks_respect_zero_as_unlimited() {
        let limits = LimitsConfig {
            max_depth: 0,
            max_aliases: 1,
            ..LimitsConfig::default()
        };
        let s = OperationStats {
            depth: 1_000,
            aliases: 1,
            ..OperationStats::default()
        };
        assert!(check_stats(&s, &limits).is_ok());
        let s = OperationStats { aliases: 2, ..s };
        let v = check_stats(&s, &limits).unwrap_err();
        assert_eq!((v.limit, v.max, v.actual), ("aliases", 1, 2));
        assert_eq!(v.to_string(), "number of aliases 2 exceeds the limit of 1");

        let limits = LimitsConfig {
            max_query_bytes: 4,
            ..LimitsConfig::default()
        };
        assert_eq!(
            check_text("{ a }", &limits).unwrap_err().limit,
            "query_bytes"
        );
        let limits = LimitsConfig {
            max_nesting: 2,
            ..LimitsConfig::default()
        };
        let v = check_text("{ a { b { c } } }", &limits).unwrap_err();
        assert_eq!((v.limit, v.max), ("nesting", 2));
        assert!(check_text("{ a { b } }", &limits).is_ok());
    }
}
