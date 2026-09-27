//! Axis 3 — security.
//!
//! Secrets (endpoint keys, tokens) must never reach argv, logs, the journal or a debug print, and
//! documents that carry them must be owner-only on disk. And marion — tests included — never
//! starts a harness login: it inherits the operator's.
//!
//! Rules, each emitted under its own name:
//! - `secret-debug`: a type named for a secret, holding a string, derives `Debug`, or implements
//!   `Display`; or any type deriving `Debug` carries a string field named like a secret.
//! - `secret-format`: a formatting/logging macro reads a secret-named value.
//! - `secret-doc-mode`: a file that writes credential-bearing documents creates one without an
//!   owner-only mode in the same function.
//! - `secret-argv`: a secret-named value, or a `--api-key`-style flag, goes onto a command line.
//! - `test-login`: test code spells a login command or device flow.

use proc_macro2::{TokenStream, TokenTree};
use syn::spanned::Spanned;

use crate::scan::{Axis, FnFrame, Scanner};

pub const FIX: &str = "\
Keep secrets out of every printable and persistent surface: give a secret its own type with a \
hand-written redacting `Debug` (and no `Display`), format only a redacted form or a presence bit, \
pass secrets to children through the environment (never argv), and create files that carry keys \
or tokens with `OpenOptions` + `std::os::unix::fs::OpenOptionsExt::mode(0o600)`. Tests never run a \
login or device flow — a harness without auth is BLOCKED and reported with the command the user \
should run. If a finding is a verified false positive, add `path:item  <why it is safe>` to \
checks/security.allow.";

/// CamelCase words that make a type name a secret's.
const SECRET_TYPE_WORDS: &[&str] = &[
    "Secret",
    "Secrets",
    "Key",
    "Keys",
    "Token",
    "Tokens",
    "Credential",
    "Credentials",
    "Password",
];

/// Exact identifiers treated as a secret's value. Not bare `key` (in this codebase almost always
/// an environment variable's *name* or a map key), not `auth` (a mode), not `credential(s)`
/// (credential *ids* such as `openrouter:work`) — measured on the tree, those were all noise.
const SECRET_IDENTS: &[&str] = &[
    "api_key", "apikey", "token", "secret", "password", "passwd", "bearer",
];

/// Methods that hand out a secret's plaintext: formatting their result is exposure by definition.
const EXPOSING_METHODS: &[&str] = &["expose", "expose_secret", "reveal"];

/// A type named `CredentialId`, `TokenKind` or `KeyError` names *about* a secret, not one.
const NON_SECRET_TAIL_WORDS: &[&str] = &["Id", "Error", "Kind", "Name", "Label", "Ref", "Source"];
const SECRET_SUFFIXES: &[&str] = &["_api_key", "_apikey", "_token", "_secret", "_password"];

/// A value read through one of these is not the secret itself.
const HARMLESS_METHODS: &[&str] = &["len", "is_empty", "is_some", "is_none", "is_ok", "is_err"];

const FORMAT_MACROS: &[&str] = &[
    "format",
    "format_args",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "unreachable",
    "todo",
    "info",
    "debug",
    "warn",
    "error",
    "trace",
    "event",
    "log",
    "bail",
    "anyhow",
    "ensure",
];

/// String literals naming a document that carries credentials. A file that spells one, or that
/// asks an adapter for its `config_files` (which embed the node's bridge token and, in endpoint
/// mode, the provider key), writes such documents.
const SECRET_DOC_MARKERS: &[&str] = &["mcp.json", "credentials", "auth.json", "oauth"];
const SECRET_DOC_CALLS: &[&str] = &["config_files"];

/// Test-code literals that start a login: an argv token (`"login"`, `"/login"`, a device-flow
/// flag) or a short command line (`"codex login"`, `"gemini auth login"`). Prose that mentions a
/// login ("runs on the operator's own login") is not one; neither is an expected message checked
/// with `contains`/`assert!` — the scanner does not call this in those positions.
fn is_login_literal(v: &str) -> bool {
    const TOKENS: &[&str] = &[
        "login",
        "/login",
        "setup-token",
        "--device-auth",
        "--device-code",
        "device-code",
    ];
    let t = v.trim();
    let words: Vec<&str> = t.split_whitespace().collect();
    let command_like = words.len() <= 4
        && words.iter().all(|w| {
            w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-/_.:".contains(c))
        });
    TOKENS.contains(&t) || (command_like && words.iter().any(|w| TOKENS.contains(w)))
}

pub fn is_secret_ident(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_IDENTS.contains(&n.as_str()) || SECRET_SUFFIXES.iter().any(|s| n.ends_with(s))
}

fn camel_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        if c != '_' {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn is_secret_type_name(name: &str) -> bool {
    let words = camel_words(name);
    words
        .iter()
        .any(|w| SECRET_TYPE_WORDS.contains(&w.as_str()))
        && !words
            .last()
            .is_some_and(|w| NON_SECRET_TAIL_WORDS.contains(&w.as_str()))
}

fn holds_string(ty: &syn::Type) -> bool {
    let t = quote::quote!(#ty).to_string();
    t.contains("Vec < u8 >")
        || t.split(|c: char| !c.is_alphanumeric())
            .any(|w| ["String", "str", "OsString", "Zeroizing", "SecretString"].contains(&w))
}

fn derives_debug(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("derive")
            && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string().split(|c: char| !c.is_alphanumeric()).any(|w| w == "Debug"))
    })
}

fn check_fields(
    s: &mut Scanner,
    owner: &str,
    fields: &syn::Fields,
    type_is_secret: bool,
    debug: bool,
) {
    if !debug {
        return;
    }
    for (i, f) in fields.iter().enumerate() {
        if !holds_string(&f.ty) {
            continue;
        }
        let fname = f
            .ident
            .as_ref()
            .map(|i| i.to_string())
            .unwrap_or_else(|| i.to_string());
        if type_is_secret
            || f.ident
                .as_ref()
                .is_some_and(|i| is_secret_ident(&i.to_string()))
        {
            s.emit(
                Axis::Security,
                "secret-debug",
                f.span(),
                format!("`{owner}` derives Debug and prints its string field `{fname}` unredacted"),
            );
        }
    }
}

pub fn check_struct(s: &mut Scanner, i: &syn::ItemStruct) {
    let name = i.ident.to_string();
    check_fields(
        s,
        &name,
        &i.fields,
        is_secret_type_name(&name),
        derives_debug(&i.attrs),
    );
}

/// Variants are judged by the enum's name and their fields' names only: a variant name such as
/// `BadCredential` or `DeclarationKey` describes an error or a config shape, not what it holds.
pub fn check_enum(s: &mut Scanner, i: &syn::ItemEnum) {
    let name = i.ident.to_string();
    let debug = derives_debug(&i.attrs);
    let secret = is_secret_type_name(&name);
    for v in &i.variants {
        let owner = format!("{name}::{}", v.ident);
        check_fields(s, &owner, &v.fields, secret, debug);
    }
}

/// `impl Display for <secret type>` — a secret has no display form.
pub fn check_impl(s: &mut Scanner, i: &syn::ItemImpl) {
    let Some((_, trait_path, _)) = &i.trait_ else {
        return;
    };
    if trait_path
        .segments
        .last()
        .is_none_or(|seg| seg.ident != "Display")
    {
        return;
    }
    let ty = &i.self_ty;
    let name = quote::quote!(#ty).to_string();
    let last = name
        .split(|c: char| !c.is_alphanumeric())
        .rfind(|w| !w.is_empty())
        .unwrap_or("");
    if is_secret_type_name(last) {
        s.emit(
            Axis::Security,
            "secret-debug",
            i.span(),
            format!("`{last}` is named for a secret and implements Display"),
        );
    }
}

/// Secret-named identifiers in a token stream, skipping `x.len()`-style harmless reads.
fn secret_idents(tokens: &TokenStream) -> Vec<String> {
    let flat: Vec<TokenTree> = flatten(tokens);
    let mut out = Vec::new();
    for (idx, t) in flat.iter().enumerate() {
        let TokenTree::Ident(id) = t else { continue };
        let name = id.to_string();
        if !is_secret_ident(&name) {
            continue;
        }
        let harmless = matches!(flat.get(idx + 1), Some(TokenTree::Punct(p)) if p.as_char() == '.')
            && matches!(flat.get(idx + 2), Some(TokenTree::Ident(m)) if HARMLESS_METHODS.contains(&m.to_string().as_str()));
        // `key = value` inside a macro is a named format argument; its value is scanned too.
        if !harmless {
            out.push(name);
        }
    }
    // `x.expose()`: plaintext handed out on purpose.
    for w in flat.windows(2) {
        if let (TokenTree::Punct(p), TokenTree::Ident(m)) = (&w[0], &w[1])
            && p.as_char() == '.'
            && EXPOSING_METHODS.contains(&m.to_string().as_str())
        {
            out.push(format!(".{m}()"));
        }
    }
    // Inline `{name}` / `{name:?}` captures inside format strings.
    for t in &flat {
        if let TokenTree::Literal(l) = t
            && let Ok(syn::Lit::Str(s)) = syn::parse_str::<syn::Lit>(&l.to_string())
        {
            out.extend(
                inline_captures(&s.value())
                    .into_iter()
                    .filter(|n| is_secret_ident(n)),
            );
        }
    }
    out
}

fn flatten(tokens: &TokenStream) -> Vec<TokenTree> {
    let mut out = Vec::new();
    for t in tokens.clone() {
        if let TokenTree::Group(g) = &t {
            out.extend(flatten(&g.stream()));
        } else {
            out.push(t);
        }
    }
    out
}

fn inline_captures(fmt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let name: String = chars
            .clone()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !name.chars().next().unwrap().is_ascii_digit() {
            out.push(name);
        }
    }
    out
}

pub fn check_macro(s: &mut Scanner, name: &str, m: &syn::Macro) {
    if !FORMAT_MACROS.contains(&name) || s.in_redaction_helper() {
        return;
    }
    let mut found = secret_idents(&m.tokens);
    found.sort();
    found.dedup();
    for f in found {
        s.emit(
            Axis::Security,
            "secret-format",
            m.span(),
            format!("`{name}!` formats the secret-named value `{f}`"),
        );
    }
}

fn is_write_call(path: &[String]) -> bool {
    let tail: Vec<&str> = path.iter().rev().take(2).map(String::as_str).collect();
    matches!(
        tail.as_slice(),
        ["write", "fs"] | ["create", "File"] | ["create_new", "File"]
    )
}

pub fn check_call(s: &mut Scanner, c: &syn::ExprCall) {
    let syn::Expr::Path(p) = &*c.func else { return };
    let path: Vec<String> = p
        .path
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect();
    if is_write_call(&path) {
        let line = crate::scan::line(c.span());
        let what = path.join("::");
        if let Some(f) = s.current_fn() {
            f.writes.push((line, what));
        }
    }
}

pub fn check_method(s: &mut Scanner, m: &syn::ExprMethodCall) {
    let method = m.method.to_string();
    match method.as_str() {
        "mode" | "set_permissions" | "set_mode" => {
            if let Some(f) = s.current_fn() {
                f.sets_mode = true;
            }
        }
        "create" | "create_new" if m.args.len() == 1 => {
            // `OpenOptions::new()….create(true)`.
            let line = crate::scan::line(m.span());
            if let Some(f) = s.current_fn() {
                f.writes.push((line, format!("OpenOptions::{method}")));
            }
        }
        "arg" | "args" => {
            let args = &m.args;
            let tokens = quote::quote!(#args);
            let mut bad = secret_idents(&tokens);
            for t in flatten(&tokens) {
                if let TokenTree::Literal(l) = t
                    && let Ok(syn::Lit::Str(lit)) = syn::parse_str::<syn::Lit>(&l.to_string())
                    && let Some(flag) = lit.value().strip_prefix("--")
                {
                    let flag = flag.split('=').next().unwrap_or("").replace('-', "_");
                    if is_secret_ident(&flag) {
                        bad.push(format!("--{}", flag.replace('_', "-")));
                    }
                }
            }
            bad.sort();
            bad.dedup();
            for b in bad {
                s.emit(
                    Axis::Security,
                    "secret-argv",
                    m.span(),
                    format!("`.{method}(…)` puts `{b}` on a command line, where `ps` shows it"),
                );
            }
        }
        _ => {}
    }
}

pub fn end_of_fn(s: &mut Scanner, frame: &FnFrame) {
    if s.in_test() || !s.facts.secret_docs || frame.sets_mode {
        return;
    }
    for (line, what) in &frame.writes {
        s.out.push(crate::scan::Finding {
            axis: Axis::Security,
            file: s.file.to_string(),
            item: s.item(),
            line: *line,
            rule: "secret-doc-mode",
            detail: format!(
                "`{what}` in a module that writes credential-bearing documents, with no owner-only mode set"
            ),
        });
    }
}

pub fn check_test_literal(s: &mut Scanner, l: &syn::LitStr) {
    if s.assert_depth > 0 {
        return;
    }
    let v = l.value();
    if is_login_literal(&v) {
        s.emit(
            Axis::Security,
            "test-login",
            l.span(),
            format!("test code spells the login flow {v:?}"),
        );
    }
}

/// Does this file write documents that carry credentials? Decided from its string literals.
pub fn writes_secret_docs(file: &syn::File) -> bool {
    use syn::visit::Visit;
    #[derive(Default)]
    struct Markers(bool);
    impl<'ast> Visit<'ast> for Markers {
        fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
        fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
            if SECRET_DOC_CALLS.contains(&m.method.to_string().as_str()) {
                self.0 = true;
            }
            syn::visit::visit_expr_method_call(self, m);
        }
        fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
            if !crate::scan::is_test_attrs(&i.attrs) {
                syn::visit::visit_item_mod(self, i);
            }
        }
        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            if !crate::scan::is_test_attrs(&i.attrs) {
                syn::visit::visit_item_fn(self, i);
            }
        }
        fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
            let v = l.value();
            if SECRET_DOC_MARKERS.iter().any(|m| v.contains(m)) {
                self.0 = true;
            }
        }
    }
    let mut m = Markers::default();
    m.visit_file(file);
    m.0
}
