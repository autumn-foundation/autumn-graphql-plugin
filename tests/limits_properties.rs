//! Property tests for the invariants documented on `autumn_plugin_graphql::limits`:
//! faithfulness to naive fragment expansion, cycle safety, totality on
//! arbitrary input, and the limit decision rule.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::format_push_string,
    missing_docs
)]

use async_graphql::parser::parse_query;
use autumn_plugin_graphql::config::LimitsConfig;
use autumn_plugin_graphql::limits::{
    AnalysisError, OperationStats, analyze, check_stats, check_text, lexical_nesting,
};
use proptest::prelude::*;

/// A selection in a generated document. `Spread(i)` refers to fragment `i`.
#[derive(Debug, Clone)]
enum Sel {
    Field { alias: bool, children: Vec<Self> },
    Spread(usize),
    Inline(Vec<Self>),
}

fn selection(max_fragment: usize) -> impl Strategy<Value = Sel> {
    let leaf = prop_oneof![
        3 => any::<bool>().prop_map(|alias| Sel::Field { alias, children: vec![] }),
        1 => (0..max_fragment.max(1)).prop_map(Sel::Spread),
    ];
    leaf.prop_recursive(4, 40, 4, |inner| {
        prop_oneof![
            (any::<bool>(), prop::collection::vec(inner.clone(), 1..4))
                .prop_map(|(alias, children)| Sel::Field { alias, children }),
            prop::collection::vec(inner, 1..4).prop_map(Sel::Inline),
        ]
    })
}

/// An operation plus `n` fragments. Acyclic by construction: a spread
/// inside fragment `i` is rewritten to point at a fragment `j > i`, or
/// dropped (replaced by a leaf field) when there is none.
#[derive(Debug, Clone)]
struct Doc {
    operation: Vec<Sel>,
    fragments: Vec<Vec<Sel>>,
}

fn doc() -> impl Strategy<Value = Doc> {
    (0usize..5).prop_flat_map(|n| {
        let set = move || prop::collection::vec(selection(n), 1..4);
        (set(), prop::collection::vec(set(), n..=n)).prop_map(move |(operation, fragments)| {
            let fragments = fragments
                .into_iter()
                .enumerate()
                .map(|(i, set)| forward_only(set, i, n))
                .collect();
            let operation = forward_only(operation, usize::MAX, n);
            Doc {
                operation,
                fragments,
            }
        })
    })
}

fn forward_only(set: Vec<Sel>, owner: usize, n: usize) -> Vec<Sel> {
    set.into_iter()
        .map(|sel| match sel {
            Sel::Spread(j) if n == 0 => Sel::Field {
                alias: j % 2 == 0,
                children: vec![],
            },
            Sel::Spread(j) if owner == usize::MAX => Sel::Spread(j % n),
            Sel::Spread(j) => {
                let remaining = n - owner - 1;
                if remaining == 0 {
                    Sel::Field {
                        alias: false,
                        children: vec![],
                    }
                } else {
                    Sel::Spread(owner + 1 + j % remaining)
                }
            }
            Sel::Field { alias, children } => Sel::Field {
                alias,
                children: forward_only(children, owner, n),
            },
            Sel::Inline(children) => Sel::Inline(forward_only(children, owner, n)),
        })
        .collect()
}

fn render_set(set: &[Sel], out: &mut String) {
    out.push_str("{ ");
    for sel in set {
        match sel {
            Sel::Field { alias, children } => {
                if *alias {
                    out.push_str("a: ");
                }
                out.push('f');
                if !children.is_empty() {
                    out.push(' ');
                    render_set(children, out);
                }
                out.push(' ');
            }
            Sel::Spread(i) => out.push_str(&format!("...F{i} ")),
            Sel::Inline(children) => {
                out.push_str("... ");
                render_set(children, out);
                out.push(' ');
            }
        }
    }
    out.push('}');
}

fn render(doc: &Doc) -> String {
    let mut out = String::from("query ");
    render_set(&doc.operation, &mut out);
    for (i, fragment) in doc.fragments.iter().enumerate() {
        out.push_str(&format!("\nfragment F{i} on Q "));
        render_set(fragment, &mut out);
    }
    out
}

/// Naive expansion: follow every spread. Exponential in the worst case,
/// which is fine for the small documents generated here.
fn naive(doc: &Doc, set: &[Sel]) -> (usize, usize, usize, usize) {
    // (depth, aliases, flat fields, fields)
    let mut acc = (0, 0, 0, 0);
    for sel in set {
        let (d, a, flat, f) = match sel {
            Sel::Field { alias, children } => {
                let (d, a, _, f) = naive(doc, children);
                (d + 1, a + usize::from(*alias), 1, f + 1)
            }
            Sel::Inline(children) => naive(doc, children),
            Sel::Spread(i) => naive(doc, &doc.fragments[*i]),
        };
        acc = (acc.0.max(d), acc.1 + a, acc.2 + flat, acc.3 + f);
    }
    acc
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn memoized_analysis_equals_naive_expansion(doc in doc()) {
        let text = render(&doc);
        let parsed = parse_query(&text).unwrap();
        let stats = analyze(&parsed, None).unwrap();
        let (depth, aliases, root_fields, fields) = naive(&doc, &doc.operation);
        prop_assert_eq!(
            stats,
            OperationStats { depth, aliases, root_fields, fields, directives: 0, introspection: false },
            "{}", text
        );
    }

    #[test]
    fn a_reachable_cycle_is_always_reported(len in 1usize..8, extra in selection(1)) {
        // op → F0 → F1 → … → F(len-1) → F0
        let mut text = String::from("query { ...F0 }\n");
        for i in 0..len {
            let mut body = String::new();
            render_set(std::slice::from_ref(&extra), &mut body);
            let body = body.replace("...F0", "f");
            text.push_str(&format!(
                "fragment F{i} on Q {{ x {body} ...F{} }}\n",
                (i + 1) % len
            ));
        }
        let parsed = parse_query(&text).unwrap();
        prop_assert!(matches!(analyze(&parsed, None), Err(AnalysisError::FragmentCycle(_))), "{}", text);
    }

    #[test]
    fn analysis_is_total_on_arbitrary_documents(text in "[a-cF0-2{}().:\\[\\]\"# \n]{0,200}") {
        let _ = lexical_nesting(&text, usize::MAX);
        let _ = check_text(&text, &LimitsConfig::default());
        if let Ok(parsed) = parse_query(&text) {
            let _ = analyze(&parsed, None);
        }
    }

    #[test]
    fn nesting_matches_a_stack_on_plain_text(text in "[a{}()\\[\\] ]{0,300}") {
        let mut depth = 0usize;
        let mut max = 0usize;
        for c in text.chars() {
            match c {
                '{' | '(' | '[' => { depth += 1; max = max.max(depth); }
                '}' | ')' | ']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        prop_assert_eq!(lexical_nesting(&text, usize::MAX), max);
        prop_assert_eq!(lexical_nesting(&text, 3), max.min(3));
    }

    #[test]
    fn a_limit_is_violated_iff_a_nonzero_maximum_is_exceeded(
        stats in (0usize..20, 0usize..20, 0usize..20, 0usize..20, 0usize..20),
        limits in (0usize..20, 0usize..20, 0usize..20, 0usize..20, 0usize..20),
    ) {
        let s = OperationStats {
            depth: stats.0, aliases: stats.1, root_fields: stats.2, fields: stats.3,
            directives: stats.4, introspection: false,
        };
        let mut l = LimitsConfig::default();
        l.max_depth = limits.0;
        l.max_aliases = limits.1;
        l.max_root_fields = limits.2;
        l.max_fields = limits.3;
        l.max_directives = limits.4;
        let pairs = [
            (s.depth, l.max_depth), (s.aliases, l.max_aliases), (s.root_fields, l.max_root_fields),
            (s.fields, l.max_fields), (s.directives, l.max_directives),
        ];
        let expected = pairs.iter().any(|(actual, max)| *max != 0 && actual > max);
        let result = check_stats(&s, &l);
        prop_assert_eq!(result.is_err(), expected);
        if let Err(v) = result {
            prop_assert!(v.max != 0 && v.actual > v.max);
        }
    }
}
