//! Axis 1 — harness generality.
//!
//! marion's promise is that a harness is a registry row plus, at most, one enumerated strategy.
//! Code outside the row files that branches on *which* harness it is — a `Harness::Codex`
//! pattern, `harness == Harness::Acp`, `name == "claude"`, a baked-in `"codex-impl"` — is the
//! per-harness hardcoding that promise excludes. Constructing a `Harness` value is not a branch
//! and is not flagged; deciding on one is.

use proc_macro2::TokenStream;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::scan::{Axis, Scanner, Vocabulary};

/// Files where harness-specific code is the point: one row per harness, the ACP row, and the enum's
/// own definition (`as_str`, `cli_name`, `FromStr`, `ALL`). `pi.rs` is listed ahead of its row
/// landing so the row file is exempt the day it arrives.
pub const ROW_FILES: &[&str] = &[
    "crates/marion-core/src/harness.rs",
    "crates/marion-harness/src/claude_code.rs",
    "crates/marion-harness/src/codex.rs",
    "crates/marion-harness/src/opencode.rs",
    "crates/marion-harness/src/copilot.rs",
    "crates/marion-harness/src/goose.rs",
    "crates/marion-harness/src/cline.rs",
    "crates/marion-harness/src/qwen.rs",
    "crates/marion-harness/src/acp.rs",
    "crates/marion-harness/src/antigravity.rs",
    "crates/marion-harness/src/pi.rs",
];

/// Agent names not (yet) in the enum's wire table, which a string comparison could still branch on.
const EXTRA_NAMES: &[&str] = &[
    "claude",
    "antigravity",
    "pi",
    "cursor",
    "aider",
    "amp",
    "kiro",
];

pub const FIX: &str = "\
Move the per-harness behavior into the harness's row file (crates/marion-harness/src/<harness>.rs) \
as row data, or into an enumerated strategy that every row states, with a sweep test over \
`Harness::ALL` proving each row answers it. Code outside the rows should ask the row, never test \
which harness it is. If this genuinely cannot be generalized yet, add `path:item  <reason>` to \
checks/generality.allow — the list is meant to shrink, so say what would remove the entry.";

/// Variants from the `Harness` enum, and wire/CLI names from the string literals in its `as_str`
/// and `cli_name`, plus [`EXTRA_NAMES`].
pub fn vocabulary(harness_rs: &syn::File) -> Vocabulary {
    let mut variants = Vec::new();
    let mut names: Vec<String> = EXTRA_NAMES.iter().map(|s| s.to_string()).collect();
    for item in &harness_rs.items {
        match item {
            syn::Item::Enum(e) if e.ident == "Harness" => {
                variants.extend(e.variants.iter().map(|v| v.ident.to_string()));
            }
            syn::Item::Impl(imp) => {
                for ii in &imp.items {
                    let syn::ImplItem::Fn(f) = ii else { continue };
                    if f.sig.ident == "as_str" || f.sig.ident == "cli_name" {
                        let mut lits = LitCollector::default();
                        lits.visit_block(&f.block);
                        names.extend(lits.0);
                    }
                }
            }
            _ => {}
        }
    }
    names.sort();
    names.dedup();
    assert!(
        variants.len() >= 2 && names.len() >= variants.len(),
        "three-axis: could not read the Harness enum from crates/marion-core/src/harness.rs"
    );
    let mut words: Vec<String> = variants
        .iter()
        .filter(|v| v.as_str() != "Acp")
        .cloned()
        .chain(EXTRA_WORDS.iter().map(|w| w.to_string()))
        .collect();
    words.sort();
    words.dedup();
    Vocabulary {
        variants,
        names,
        words,
    }
}

/// CamelCase prefixes that name one vendor's harness in a type or variant, beyond the enum's own
/// variants. `Acp` is a protocol every ACP agent shares, so it is not one of them.
const EXTRA_WORDS: &[&str] = &["Claude", "Agy"];

/// Environment-variable prefixes that belong to one vendor's harness or API.
const VENDOR_ENV_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "CODEX_",
    "GEMINI_",
    "GOOGLE_",
    "OPENCODE_",
    "COPILOT_",
    "GITHUB_COPILOT",
    "GOOSE_",
    "CLINE_",
    "QWEN_",
    "DASHSCOPE_",
];

/// `ClaudeCodeAdapter`, `CodexReleases`, `Copilot`: an identifier that starts with a harness word
/// at a CamelCase boundary.
pub fn is_harness_named(ident: &str, vocab: &Vocabulary) -> bool {
    vocab.words.iter().any(|w| {
        ident
            .strip_prefix(w.as_str())
            .is_some_and(|rest| rest.chars().next().is_none_or(|c| c.is_ascii_uppercase()))
    })
}

/// `ACP_COMMAND_PREFIX`, `CLAUDE_DEFAULT`: a SCREAMING constant named for a harness, compared
/// against as a stand-in for its literal.
fn is_harness_const(path: &syn::Path, vocab: &Vocabulary) -> bool {
    let Some(last) = path.segments.last().map(|s| s.ident.to_string()) else {
        return false;
    };
    if last.chars().any(|c| c.is_ascii_lowercase()) {
        return false;
    }
    last.split('_').any(|part| {
        vocab
            .names
            .iter()
            .any(|n| !n.contains('-') && n.to_ascii_uppercase() == part)
    })
}

#[derive(Default)]
struct LitCollector(Vec<String>);
impl<'ast> Visit<'ast> for LitCollector {
    fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
        self.0.push(l.value());
    }
}

/// `Harness::X` (or `…::Harness::X`) naming a variant.
fn variant_of(path: &syn::Path, vocab: &Vocabulary) -> Option<String> {
    let n = path.segments.len();
    if n < 2 || path.segments[n - 2].ident != "Harness" {
        return None;
    }
    let last = path.segments[n - 1].ident.to_string();
    vocab.variants.contains(&last).then_some(last)
}

/// Every `Harness::X` value inside an expression.
struct VariantFinder<'v> {
    vocab: &'v Vocabulary,
    found: Vec<String>,
}
impl<'ast> Visit<'ast> for VariantFinder<'_> {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        if let Some(v) = variant_of(p, self.vocab) {
            self.found.push(v);
        }
        syn::visit::visit_path(self, p);
    }
}

fn variants_in(e: &syn::Expr, vocab: &Vocabulary) -> Vec<String> {
    let mut f = VariantFinder {
        vocab,
        found: Vec::new(),
    };
    f.visit_expr(e);
    f.found
}

/// A string literal, seen through `&`, parentheses and a trailing `.as_str()`/`.as_ref()`.
fn str_lit(e: &syn::Expr) -> Option<&syn::LitStr> {
    match e {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Some(s),
        syn::Expr::Reference(r) => str_lit(&r.expr),
        syn::Expr::Paren(p) => str_lit(&p.expr),
        _ => None,
    }
}

/// A harness name, or an agent-type name built on one (`codex-impl`, `claude-orchestrator`).
pub fn is_harness_name(s: &str, vocab: &Vocabulary) -> bool {
    vocab.names.iter().any(|n| n == s) || is_agent_type_name(s, vocab)
}

fn is_agent_type_name(s: &str, vocab: &Vocabulary) -> bool {
    if vocab.names.iter().any(|n| n == s) {
        return false;
    }
    vocab.names.iter().any(|n| {
        s.strip_prefix(n.as_str())
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            })
    })
}

fn applies(s: &Scanner) -> bool {
    !s.facts.row_file && s.harness_impl_depth == 0
}

/// An `impl` for a harness-named type (`impl HarnessAdapter for CodexAdapter`) outside the row
/// files is one finding for the whole block — the block is the per-harness code — and suppresses
/// the finer findings inside it. Returns whether it did.
pub fn enter_impl(s: &mut Scanner, i: &syn::ItemImpl) -> bool {
    if !applies(s) {
        return false;
    }
    let syn::Type::Path(tp) = &*i.self_ty else {
        return false;
    };
    let Some(name) = tp.path.segments.last().map(|seg| seg.ident.to_string()) else {
        return false;
    };
    if !is_harness_named(&name, s.vocab) {
        return false;
    }
    s.emit(
        Axis::Generality,
        "harness-impl-outside-row",
        i.impl_token.span,
        format!("`{name}` is one harness's implementation, living outside its row file"),
    );
    true
}

pub fn check_binary(s: &mut Scanner, b: &syn::ExprBinary) {
    if !applies(s) || !matches!(b.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
        return;
    }
    for side in [&*b.left, &*b.right] {
        for v in variants_in(side, s.vocab) {
            s.emit(
                Axis::Generality,
                "harness-variant-compare",
                b.span(),
                format!("compares against `Harness::{v}`"),
            );
        }
        if let Some(l) = str_lit(side).filter(|l| is_harness_name(&l.value(), s.vocab)) {
            s.emit(
                Axis::Generality,
                "harness-name-compare",
                b.span(),
                format!("compares a value against the harness name {:?}", l.value()),
            );
        }
        if let Some(c) = const_path(side).filter(|p| is_harness_const(p, s.vocab)) {
            s.emit(
                Axis::Generality,
                "harness-name-compare",
                b.span(),
                format!(
                    "compares a value against the harness-named constant `{}`",
                    path_str(c)
                ),
            );
        }
    }
}

fn const_path(e: &syn::Expr) -> Option<&syn::Path> {
    match e {
        syn::Expr::Path(p) => Some(&p.path),
        syn::Expr::Reference(r) => const_path(&r.expr),
        syn::Expr::Paren(p) => const_path(&p.expr),
        _ => None,
    }
}

fn path_str(p: &syn::Path) -> String {
    p.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

const MEMBERSHIP_METHODS: &[&str] = &["contains", "eq", "ne", "starts_with", "ends_with"];
const STRING_TEST_METHODS: &[&str] = &[
    "starts_with",
    "ends_with",
    "contains",
    "eq",
    "ne",
    "eq_ignore_ascii_case",
    "strip_prefix",
    "strip_suffix",
];

pub fn check_method(s: &mut Scanner, m: &syn::ExprMethodCall) {
    if !applies(s) {
        return;
    }
    let method = m.method.to_string();
    if MEMBERSHIP_METHODS.contains(&method.as_str()) {
        let mut found = variants_in(&m.receiver, s.vocab);
        for a in &m.args {
            found.extend(variants_in(a, s.vocab));
        }
        for v in found {
            s.emit(
                Axis::Generality,
                "harness-variant-compare",
                m.span(),
                format!("tests membership of `Harness::{v}` with `.{method}(…)`"),
            );
        }
    }
    if STRING_TEST_METHODS.contains(&method.as_str()) {
        for a in &m.args {
            if let Some(l) = str_lit(a).filter(|l| is_harness_name(&l.value(), s.vocab)) {
                s.emit(
                    Axis::Generality,
                    "harness-name-compare",
                    m.span(),
                    format!("`.{method}({:?})` tests for a harness name", l.value()),
                );
            }
            if let Some(c) = const_path(a).filter(|p| is_harness_const(p, s.vocab)) {
                s.emit(
                    Axis::Generality,
                    "harness-name-compare",
                    m.span(),
                    format!(
                        "`.{method}({})` tests for a harness-named constant",
                        path_str(c)
                    ),
                );
            }
        }
    }
}

pub fn check_pat(s: &mut Scanner, p: &syn::Pat) {
    if !applies(s) {
        return;
    }
    let path = match p {
        syn::Pat::Path(pp) => Some(&pp.path),
        syn::Pat::TupleStruct(t) => Some(&t.path),
        syn::Pat::Struct(st) => Some(&st.path),
        _ => None,
    };
    if let Some(v) = path.and_then(|path| variant_of(path, s.vocab)) {
        s.emit(
            Axis::Generality,
            "harness-variant-pattern",
            p.span(),
            format!("matches on `Harness::{v}`"),
        );
    }
    if let Some(path) = path
        && variant_of(path, s.vocab).is_none()
        && path.segments.len() >= 2
        && is_harness_named(&path.segments.last().unwrap().ident.to_string(), s.vocab)
    {
        s.emit(
            Axis::Generality,
            "harness-named-variant",
            p.span(),
            format!(
                "matches on `{}`, a variant named for one harness — name strategies for their mechanism",
                path_str(path)
            ),
        );
    }
    if let syn::Pat::Lit(l) = p
        && let syn::Lit::Str(lit) = &l.lit
        && is_harness_name(&lit.value(), s.vocab)
    {
        s.emit(
            Axis::Generality,
            "harness-name-pattern",
            p.span(),
            format!("matches on the harness name {:?}", lit.value()),
        );
    }
}

/// A baked-in agent-type name (`"codex-impl"`) anywhere in production code: a default or a
/// lookup keyed on one harness, whether or not it sits in a conditional.
pub fn check_literal(s: &mut Scanner, l: &syn::LitStr) {
    if !applies(s) {
        return;
    }
    let v = l.value();
    if VENDOR_ENV_PREFIXES.iter().any(|p| v.starts_with(p))
        && v.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        s.emit(
            Axis::Generality,
            "vendor-env-literal",
            l.span(),
            format!("names the vendor-specific variable {v:?} outside the rows"),
        );
    }
    if is_agent_type_name(&v, s.vocab) {
        s.emit(
            Axis::Generality,
            "agent-type-literal",
            l.span(),
            format!("names the harness-specific agent type {:?}", l.value()),
        );
    }
}

/// `matches!(expr, pattern)` — its arguments are tokens to syn, so parse them.
pub fn parse_matches(tokens: &TokenStream) -> Option<(syn::Expr, syn::Pat)> {
    use syn::parse::Parser;
    let parser = |input: syn::parse::ParseStream| {
        let e: syn::Expr = input.parse()?;
        input.parse::<syn::Token![,]>()?;
        let p = syn::Pat::parse_multi_with_leading_vert(input)?;
        let _: TokenStream = input.parse()?;
        Ok((e, p))
    };
    parser.parse2(tokens.clone()).ok()
}
