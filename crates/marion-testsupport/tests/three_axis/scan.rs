//! One walk over each file's syntax tree, emitting findings for all three axes.
//!
//! The walker only tracks context — which item it is in, whether that item is test code, how
//! many loops enclose the current expression, whether the enclosing function sets a file mode —
//! and hands each node to the per-axis rules in `generality`, `efficiency` and `security`.
//! Keeping the context in one place is what lets every rule agree on what "test code" and "the
//! item a finding belongs to" mean.

use proc_macro2::{Span, TokenStream};
use syn::visit::{self, Visit};

use crate::walk::Source;
use crate::{efficiency, generality, security};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Axis {
    Generality,
    Efficiency,
    Security,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub axis: Axis,
    pub file: String,
    pub item: String,
    pub line: usize,
    pub rule: &'static str,
    pub detail: String,
}

impl Finding {
    /// The allowlist key: `path:item`. Item-level, not line-level, so an unrelated edit above a
    /// finding does not churn the allowlist.
    pub fn key(&self) -> String {
        format!("{}:{}", self.file, self.item)
    }
}

/// The harness vocabulary, read from the enum itself so a new variant or wire name is covered the
/// day it is added.
pub struct Vocabulary {
    pub variants: Vec<String>,
    pub names: Vec<String>,
    /// CamelCase words naming one vendor's harness (`Codex`, `Claude`), for type/variant names.
    pub words: Vec<String>,
}

/// Per-file facts a rule needs that are not about one node.
pub struct FileFacts {
    /// This file is a harness row file (or the `Harness` enum's own), where harness-specific code
    /// is the point.
    pub row_file: bool,
    /// This file writes a document that carries credentials (see `security::writes_secret_docs`).
    pub secret_docs: bool,
}

pub struct Scanner<'a> {
    pub vocab: &'a Vocabulary,
    pub file: &'a str,
    pub facts: FileFacts,
    file_test: bool,
    items: Vec<String>,
    test_depth: usize,
    pub harness_impl_depth: usize,
    /// Inside an `assert*!` or the argument of `contains`/`starts_with`/…: an expected value,
    /// not something the code does.
    pub assert_depth: usize,
    /// Inside the argv of marion's **own** binary: an argument to a test helper named `marion…`,
    /// or `.arg`/`.args` in a function that spawns marion's binary and no other program. There a
    /// `login` is marion's verb against the test's scratch store, not a harness login.
    pub marion_argv: usize,
    /// Inside a struct literal's `…_hint` field: words printed for the operator, never run.
    pub hint_depth: usize,
    pub loop_depth: usize,
    fns: Vec<FnFrame>,
    pub out: Vec<Finding>,
}

/// What a function body has done so far, for rules decided when the function ends.
#[derive(Default)]
pub struct FnFrame {
    pub name: String,
    pub writes: Vec<(usize, String)>,
    /// Set by an owner-only mode given **at open** (`OpenOptionsExt::mode`).
    pub sets_mode: bool,
    /// Lines of a `set_permissions`/`set_mode` after the file exists: a chmod after create, which
    /// leaves the file readable under the umask until it runs.
    pub chmods: Vec<usize>,
    /// Locals bound to a secret's plaintext (`let k = key.expose();`), read as secret-named for
    /// the rest of the function.
    pub exposed: Vec<String>,
    /// The body spawns marion's own binary (`CARGO_BIN_EXE_marion`) and no program named by a
    /// string literal — so every `.arg`/`.args` in it is marion's argv.
    pub runs_marion: bool,
}

pub fn scan(src: &Source, vocab: &Vocabulary, facts: FileFacts) -> Vec<Finding> {
    let mut s = Scanner {
        vocab,
        file: &src.rel,
        facts,
        file_test: src.test_file,
        items: Vec::new(),
        test_depth: 0,
        harness_impl_depth: 0,
        assert_depth: 0,
        marion_argv: 0,
        hint_depth: 0,
        loop_depth: 0,
        fns: Vec::new(),
        out: Vec::new(),
    };
    s.visit_file(&src.ast);
    s.out.sort();
    s.out.dedup();
    s.out
}

/// Whether a function body spawns marion's own binary and no other: it names
/// `CARGO_BIN_EXE_marion`, and no `Command::new` takes a string literal program.
fn runs_only_marion(body: &TokenStream) -> bool {
    fn flat(t: &TokenStream, out: &mut Vec<proc_macro2::TokenTree>) {
        for tt in t.clone() {
            match &tt {
                proc_macro2::TokenTree::Group(g) => {
                    out.push(tt.clone());
                    flat(&g.stream(), out);
                }
                _ => out.push(tt),
            }
        }
    }
    let mut toks = Vec::new();
    flat(body, &mut toks);
    let names_marion = toks.iter().any(|t| match t {
        proc_macro2::TokenTree::Literal(l) => l.to_string() == "\"CARGO_BIN_EXE_marion\"",
        _ => false,
    });
    let other_program = toks.windows(5).any(|w| {
        matches!(
            (&w[0], &w[3], &w[4]),
            (proc_macro2::TokenTree::Ident(c), proc_macro2::TokenTree::Ident(n), proc_macro2::TokenTree::Group(g))
                if c == "Command" && n == "new"
                    && g.stream().into_iter().next().is_some_and(|f| matches!(f, proc_macro2::TokenTree::Literal(_)))
        )
    });
    names_marion && !other_program
}

pub fn line(span: Span) -> usize {
    span.start().line
}

/// `#[test]`, `#[tokio::test]`-style, `#[cfg(test)]` and `#[cfg(all(test, …))]`. `cfg(not(test))`
/// is production code.
pub fn is_test_attrs(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let path = a.path();
        if path.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        if !path.is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &a.meta else {
            return false;
        };
        let text = list.tokens.to_string().replace(' ', "");
        text == "test"
            || (text.starts_with("all(") && text.contains("test") && !text.contains("not("))
    })
}

impl<'a> Scanner<'a> {
    pub fn in_test(&self) -> bool {
        self.file_test || self.test_depth > 0
    }

    pub fn item(&self) -> String {
        if self.items.is_empty() {
            "<file>".to_string()
        } else {
            self.items.join("::")
        }
    }

    /// Any enclosing function is a redaction helper, where touching the secret is the job.
    pub fn in_redaction_helper(&self) -> bool {
        self.fns.iter().any(|f| {
            let n = f.name.to_ascii_lowercase();
            ["redact", "mask", "scrub", "sanitize"]
                .iter()
                .any(|w| n.contains(w))
        })
    }

    pub fn current_fn(&mut self) -> Option<&mut FnFrame> {
        self.fns.last_mut()
    }

    pub fn emit(&mut self, axis: Axis, rule: &'static str, span: Span, detail: String) {
        self.out.push(Finding {
            axis,
            file: self.file.to_string(),
            item: self.item(),
            line: line(span),
            rule,
            detail,
        });
    }

    fn scoped<F: FnOnce(&mut Self)>(
        &mut self,
        name: Option<String>,
        attrs: &[syn::Attribute],
        f: F,
    ) {
        let test = is_test_attrs(attrs);
        if let Some(n) = &name {
            self.items.push(n.clone());
        }
        if test {
            self.test_depth += 1;
        }
        f(self);
        if test {
            self.test_depth -= 1;
        }
        if name.is_some() {
            self.items.pop();
        }
    }

    fn in_fn<F: FnOnce(&mut Self)>(
        &mut self,
        name: String,
        attrs: &[syn::Attribute],
        body: TokenStream,
        f: F,
    ) {
        self.scoped(Some(name.clone()), attrs, |s| {
            let outer_loops = std::mem::take(&mut s.loop_depth);
            s.fns.push(FnFrame {
                name,
                runs_marion: runs_only_marion(&body),
                ..FnFrame::default()
            });
            f(s);
            let frame = s.fns.pop().unwrap();
            s.loop_depth = outer_loops;
            security::end_of_fn(s, &frame);
        });
    }

    fn in_loop<F: FnOnce(&mut Self)>(&mut self, f: F) {
        self.loop_depth += 1;
        f(self);
        self.loop_depth -= 1;
    }

    /// Visit the arguments of a function-like macro as expressions where they parse as a
    /// comma-separated list (`vec![…]`, `assert!(…)`, `format!(…)`), so rules see through them.
    fn visit_macro_args(&mut self, tokens: &TokenStream) {
        use syn::parse::Parser;
        let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
        if let Ok(args) = parser.parse2(tokens.clone()) {
            for arg in &args {
                self.visit_expr(arg);
            }
        }
    }
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        syn::Type::Reference(r) => type_name(&r.elem),
        _ => "<impl>".to_string(),
    }
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    // Doc comments and `#[cfg]`s are attributes; nothing in them is code.
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        let body = &i.block;
        self.in_fn(
            i.sig.ident.to_string(),
            &i.attrs,
            quote::quote!(#body),
            |s| visit::visit_item_fn(s, i),
        );
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        let body = &i.block;
        self.in_fn(
            i.sig.ident.to_string(),
            &i.attrs,
            quote::quote!(#body),
            |s| visit::visit_impl_item_fn(s, i),
        );
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        let body = &i.default;
        self.in_fn(
            i.sig.ident.to_string(),
            &i.attrs,
            quote::quote!(#body),
            |s| visit::visit_trait_item_fn(s, i),
        );
    }

    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scoped(Some(i.ident.to_string()), &i.attrs, |s| {
            visit::visit_item_mod(s, i)
        });
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let name = type_name(&i.self_ty);
        self.scoped(Some(name), &i.attrs, |s| {
            let mut harness_impl = false;
            if !s.in_test() {
                security::check_impl(s, i);
                harness_impl = generality::enter_impl(s, i);
            }
            s.harness_impl_depth += usize::from(harness_impl);
            visit::visit_item_impl(s, i);
            s.harness_impl_depth -= usize::from(harness_impl);
        });
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.scoped(Some(i.ident.to_string()), &i.attrs, |s| {
            if !s.in_test() {
                security::check_struct(s, i);
            }
            visit::visit_item_struct(s, i)
        });
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.scoped(Some(i.ident.to_string()), &i.attrs, |s| {
            if !s.in_test() {
                security::check_enum(s, i);
            }
            visit::visit_item_enum(s, i)
        });
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.scoped(Some(i.ident.to_string()), &i.attrs, |s| {
            if !s.in_test() {
                efficiency::check_named_period(s, &i.ident, &i.expr);
            }
            visit::visit_item_const(s, i)
        });
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.scoped(Some(i.ident.to_string()), &i.attrs, |s| {
            if !s.in_test() {
                efficiency::check_named_period(s, &i.ident, &i.expr);
            }
            visit::visit_item_static(s, i)
        });
    }

    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.scoped(Some(i.ident.to_string()), &i.attrs, |s| {
            if !s.in_test() {
                efficiency::check_named_period(s, &i.ident, &i.expr);
            }
            visit::visit_impl_item_const(s, i)
        });
    }

    fn visit_expr_loop(&mut self, i: &'ast syn::ExprLoop) {
        self.in_loop(|s| visit::visit_expr_loop(s, i));
    }

    fn visit_expr_while(&mut self, i: &'ast syn::ExprWhile) {
        self.in_loop(|s| visit::visit_expr_while(s, i));
    }

    fn visit_expr_for_loop(&mut self, i: &'ast syn::ExprForLoop) {
        self.visit_expr(&i.expr);
        self.in_loop(|s| {
            s.visit_pat(&i.pat);
            s.visit_block(&i.body);
        });
    }

    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if !self.in_test() {
            efficiency::check_call(self, i);
            security::check_call(self, i);
        }
        // A test helper named for marion's binary (`marion(&home, &["login", …])`), or marion's
        // own command-line table (`cli::verb("login")`).
        let marion = matches!(&*i.func, syn::Expr::Path(p) if {
            let segs: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
            segs.first().is_some_and(|f| f == "cli")
                || segs.last().is_some_and(|n| n == "marion" || n.starts_with("marion_"))
        });
        self.visit_expr(&i.func);
        self.marion_argv += usize::from(marion);
        for arg in &i.args {
            self.visit_expr(arg);
        }
        self.marion_argv -= usize::from(marion);
    }

    fn visit_field_value(&mut self, i: &'ast syn::FieldValue) {
        let hint = matches!(&i.member, syn::Member::Named(n) if n.to_string().ends_with("_hint"));
        self.hint_depth += usize::from(hint);
        visit::visit_field_value(self, i);
        self.hint_depth -= usize::from(hint);
    }

    fn visit_local(&mut self, i: &'ast syn::Local) {
        if !self.in_test() {
            security::check_local(self, i);
        }
        visit::visit_local(self, i);
    }

    fn visit_expr_assign(&mut self, i: &'ast syn::ExprAssign) {
        if !self.in_test() {
            security::check_assign(self, i);
        }
        visit::visit_expr_assign(self, i);
    }

    fn visit_expr_struct(&mut self, i: &'ast syn::ExprStruct) {
        if !self.in_test() {
            security::check_struct_literal(self, i);
        }
        visit::visit_expr_struct(self, i);
    }

    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        if !self.in_test() {
            efficiency::check_method(self, i);
            security::check_method(self, i);
            generality::check_method(self, i);
        }
        const EXPECTATION: &[&str] = &["contains", "starts_with", "ends_with", "eq", "ne"];
        let method = i.method.to_string();
        let expectation = EXPECTATION.contains(&method.as_str());
        let marion = matches!(method.as_str(), "arg" | "args")
            && self.fns.last().is_some_and(|f| f.runs_marion);
        self.visit_expr(&i.receiver);
        self.assert_depth += usize::from(expectation);
        self.marion_argv += usize::from(marion);
        for arg in &i.args {
            self.visit_expr(arg);
        }
        self.marion_argv -= usize::from(marion);
        self.assert_depth -= usize::from(expectation);
    }

    fn visit_expr_binary(&mut self, i: &'ast syn::ExprBinary) {
        if !self.in_test() {
            generality::check_binary(self, i);
        }
        visit::visit_expr_binary(self, i);
    }

    fn visit_pat(&mut self, i: &'ast syn::Pat) {
        if !self.in_test() {
            generality::check_pat(self, i);
        }
        visit::visit_pat(self, i);
    }

    fn visit_lit_str(&mut self, i: &'ast syn::LitStr) {
        if self.in_test() {
            security::check_test_literal(self, i);
        } else {
            generality::check_literal(self, i);
        }
    }

    fn visit_macro(&mut self, i: &'ast syn::Macro) {
        let name = i
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        if !self.in_test() {
            security::check_macro(self, &name, i);
        }
        if name == "matches" {
            if let Some((expr, pat)) = generality::parse_matches(&i.tokens) {
                self.visit_expr(&expr);
                self.visit_pat(&pat);
            }
            return;
        }
        let assertion = name.starts_with("assert") || name.starts_with("debug_assert");
        self.assert_depth += usize::from(assertion);
        self.visit_macro_args(&i.tokens);
        self.assert_depth -= usize::from(assertion);
    }
}
