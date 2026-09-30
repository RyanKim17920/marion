//! Axis 3 — security.
//!
//! Secrets (endpoint keys, tokens) must never reach argv, logs, the journal or a debug print, and
//! documents that carry them must be owner-only on disk. And marion — tests included — never
//! starts a harness login: it inherits the operator's.
//!
//! Rules, each emitted under its own name:
//! - `secret-debug`: a type named for a secret, holding a string, derives `Debug`, or implements
//!   `Display`; or any type deriving `Debug` carries a string field named like a secret.
//! - `secret-format`: a formatting/logging macro reads a secret-named value, or a local bound to a
//!   secret's plaintext (`let k = key.expose();` then `format!("{k}")`).
//! - `secret-doc-mode`: a file that writes credential-bearing documents creates one without an
//!   owner-only mode set **at open** in the same function — a `set_permissions` after the create
//!   leaves a window in which the file is readable under the umask, and does not count.
//! - `secret-argv`: a secret-named value, or a `--api-key`-style flag, goes onto a command line —
//!   including a node's whole bridge (`bridge_env(…)`, whose declaration carries the node token)
//!   written into an argv-bound place: a row's config `pairs`, an ACP agent's `agent_args`, an
//!   invocation's `args`, a native `argv_prefix`.
//! - `token-carrier`: a harness row (`HarnessSpec { … }`) whose MCP route rides argv in some auth
//!   mode, or an ACP row (`Agent { … }`) declaring on argv, without a token carrier that withholds
//!   the node token from that declaration — so the token would be embedded in an argv string.
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

/// Types named for how a secret travels, exempt **by exact name**: `TokenCarrier` states which
/// channel a row's node token rides and holds no token. A tail word would exempt any
/// `…Carrier`, including one that holds the secret it carries.
const NON_SECRET_TYPES: &[&str] = &["TokenCarrier", "TokenCarriers"];
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
const SECRET_DOC_CALLS: &[&str] = &["config_files", "launch_documents"];

/// Fields whose contents become argv: a row's config pairs, an ACP agent's own argv, a compiled
/// invocation's argv, a native launch's argv prefix.
const ARGV_SINKS: &[&str] = &["pairs", "agent_args", "args", "argv_prefix"];

/// Methods that put a value into a collection.
const FILLING_METHODS: &[&str] = &["push", "extend", "insert", "append"];

/// The one function that builds a node's **whole** bridge — node token included — rather than a
/// declaration's view of it (`declared_bridge`, which a withholding carrier has stripped).
const FULL_BRIDGE_FNS: &[&str] = &["bridge_env"];

/// Token-carrier variants that keep the node token out of the declaration.
const WITHHOLDING_CARRIERS: &[&str] = &["InheritedEnv", "ForwardedEnv", "DeclaredFile"];

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
    if NON_SECRET_TYPES.contains(&name) {
        return false;
    }
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

/// `let k = <… .expose() …>;`: `k` holds a secret's plaintext from here to the end of the function.
pub fn check_local(s: &mut Scanner, l: &syn::Local) {
    let Some(init) = &l.init else { return };
    let expr = &init.expr;
    let exposes = flatten(&quote::quote!(#expr)).windows(2).any(|w| {
        matches!((&w[0], &w[1]), (TokenTree::Punct(p), TokenTree::Ident(m))
            if p.as_char() == '.' && EXPOSING_METHODS.contains(&m.to_string().as_str()))
    });
    if !exposes {
        return;
    }
    let pat = &l.pat;
    let names: Vec<String> = flatten(&quote::quote!(#pat))
        .into_iter()
        .filter_map(|t| match t {
            TokenTree::Ident(id) if id != "mut" && id != "ref" => Some(id.to_string()),
            _ => None,
        })
        .collect();
    if let Some(f) = s.current_fn() {
        f.exposed.extend(names);
    }
}

/// The locals of the current function bound to a secret's plaintext that `tokens` reads, directly
/// or as an inline `{name}` capture.
fn exposed_reads(s: &mut Scanner, tokens: &TokenStream) -> Vec<String> {
    let Some(exposed) = s.current_fn().map(|f| f.exposed.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in flatten(tokens) {
        match t {
            TokenTree::Ident(id) if exposed.contains(&id.to_string()) => out.push(id.to_string()),
            TokenTree::Literal(l) => {
                if let Ok(syn::Lit::Str(lit)) = syn::parse_str::<syn::Lit>(&l.to_string()) {
                    out.extend(
                        inline_captures(&lit.value())
                            .into_iter()
                            .filter(|n| exposed.contains(n)),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

pub fn check_macro(s: &mut Scanner, name: &str, m: &syn::Macro) {
    if !FORMAT_MACROS.contains(&name) || s.in_redaction_helper() {
        return;
    }
    let mut found = secret_idents(&m.tokens);
    found.extend(exposed_reads(s, &m.tokens));
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

/// Does this expression carry a node token: a whole bridge built by [`FULL_BRIDGE_FNS`], the
/// token's variable name, a `node_token` read, or an exposed secret?
fn reads_node_token(tokens: &TokenStream) -> bool {
    let flat: Vec<TokenTree> = tokens.clone().into_iter().collect();
    for (idx, t) in flat.iter().enumerate() {
        match t {
            TokenTree::Group(g) if reads_node_token(&g.stream()) => return true,
            TokenTree::Ident(id) => {
                let name = id.to_string();
                let called = matches!(
                    flat.get(idx + 1),
                    Some(TokenTree::Group(g)) if g.delimiter() == proc_macro2::Delimiter::Parenthesis
                );
                let harmless = matches!(flat.get(idx + 1), Some(TokenTree::Punct(p)) if p.as_char() == '.')
                    && matches!(flat.get(idx + 2), Some(TokenTree::Ident(m)) if HARMLESS_METHODS.contains(&m.to_string().as_str()));
                if (FULL_BRIDGE_FNS.contains(&name.as_str()) && called)
                    || name == "NODE_TOKEN_ENV"
                    || (name == "node_token" && !harmless)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    flat.windows(2).any(|w| {
        matches!((&w[0], &w[1]), (TokenTree::Punct(p), TokenTree::Ident(m))
            if p.as_char() == '.' && EXPOSING_METHODS.contains(&m.to_string().as_str()))
    })
}

/// The argv-bound field an expression names — `f.pairs`, `self.agent_args` — if it names one.
fn argv_sink(e: &syn::Expr) -> Option<String> {
    let syn::Expr::Field(f) = e else { return None };
    let syn::Member::Named(name) = &f.member else {
        return None;
    };
    let name = name.to_string();
    ARGV_SINKS.contains(&name.as_str()).then_some(name)
}

fn emit_token_argv(s: &mut Scanner, span: proc_macro2::Span, sink: &str) {
    s.emit(
        Axis::Security,
        "secret-argv",
        span,
        format!(
            "a node token reaches `{sink}`, which becomes argv where `ps` shows it; declare from \
             the carrier's view (`declared_bridge`) and let the row's token carrier set it on the \
             environment"
        ),
    );
}

/// `f.pairs = …` / `f.agent_args = …` with a node token in the value.
pub fn check_assign(s: &mut Scanner, a: &syn::ExprAssign) {
    let Some(sink) = argv_sink(&a.left) else {
        return;
    };
    let right = &a.right;
    if reads_node_token(&quote::quote!(#right)) {
        emit_token_argv(s, a.span(), &sink);
    }
}

/// Whether a token-carrier expression withholds the token: `Some(true)` for a withholding
/// variant, `Some(false)` for `Declaration`, `None` where the expression is not a carrier this
/// check can read.
fn carrier_withholds(e: &syn::Expr) -> Option<bool> {
    let last = |p: &syn::Path| p.segments.last().map(|s| s.ident.to_string());
    let name = match e {
        syn::Expr::Struct(st) => last(&st.path),
        syn::Expr::Path(p) => last(&p.path),
        _ => None,
    }?;
    if WITHHOLDING_CARRIERS.contains(&name.as_str()) {
        Some(true)
    } else if name == "Declaration" {
        Some(false)
    } else {
        None
    }
}

fn field<'a>(st: &'a syn::ExprStruct, name: &str) -> Option<&'a syn::Expr> {
    st.fields.iter().find_map(|f| match &f.member {
        syn::Member::Named(n) if n == name => Some(&f.expr),
        _ => None,
    })
}

fn path_tail(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        syn::Expr::Call(c) => path_tail(&c.func),
        syn::Expr::Struct(st) => st.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// `(canned, live)` from a `token:` value: `TokenCarriers::DECLARATION`,
/// `TokenCarriers::both(c)` or `TokenCarriers { canned, live }`. Each `None` where unreadable.
fn carriers(e: Option<&syn::Expr>) -> (Option<bool>, Option<bool>) {
    match e {
        None => (Some(false), Some(false)),
        Some(syn::Expr::Path(p))
            if p.path
                .segments
                .last()
                .is_some_and(|s| s.ident == "DECLARATION") =>
        {
            (Some(false), Some(false))
        }
        Some(syn::Expr::Call(c)) if path_tail(&c.func).as_deref() == Some("both") => {
            let one = c.args.first().and_then(carrier_withholds);
            (one, one)
        }
        Some(syn::Expr::Struct(st)) => (
            field(st, "canned").and_then(carrier_withholds),
            field(st, "live").and_then(carrier_withholds),
        ),
        Some(_) => (None, None),
    }
}

/// A `HarnessSpec { … }` or ACP `Agent { … }` literal whose argv declaration is not paired with a
/// carrier that withholds the node token.
pub fn check_struct_literal(s: &mut Scanner, st: &syn::ExprStruct) {
    let Some(kind) = st.path.segments.last().map(|seg| seg.ident.to_string()) else {
        return;
    };
    match kind.as_str() {
        "HarnessSpec" => {
            let routes = match field(st, "mcp") {
                Some(syn::Expr::Struct(r)) => r,
                _ => return,
            };
            let (canned, live) = carriers(field(st, "token"));
            for (mode, withholds) in [("canned", canned), ("live", live)] {
                let argv = field(routes, mode)
                    .and_then(path_tail)
                    .is_some_and(|t| t == "Argv");
                if argv && withholds != Some(true) {
                    s.emit(
                        Axis::Security,
                        "token-carrier",
                        st.span(),
                        format!(
                            "the row's {mode} MCP declaration rides argv and its `token` carrier \
                             does not withhold the node token, so the token is embedded in an \
                             argv string"
                        ),
                    );
                }
            }
        }
        "Agent" => {
            let Some(decl) = field(st, "declaration") else {
                return;
            };
            if path_tail(decl).as_deref() != Some("Argv") {
                return;
            }
            let withholds = match decl {
                syn::Expr::Struct(d) => field(d, "token").and_then(carrier_withholds),
                _ => None,
            };
            if withholds != Some(true) {
                s.emit(
                    Axis::Security,
                    "token-carrier",
                    st.span(),
                    "the ACP row declares its bridge on argv without a token carrier that \
                     withholds the node token, so the token is embedded in an argv string"
                        .to_string(),
                );
            }
        }
        _ => {}
    }
}

pub fn check_method(s: &mut Scanner, m: &syn::ExprMethodCall) {
    let method = m.method.to_string();
    if FILLING_METHODS.contains(&method.as_str())
        && let Some(sink) = argv_sink(&m.receiver)
    {
        let args = &m.args;
        if reads_node_token(&quote::quote!(#args)) {
            emit_token_argv(s, m.span(), &sink);
        }
    }
    match method.as_str() {
        "mode" => {
            if let Some(f) = s.current_fn() {
                f.sets_mode = true;
            }
        }
        "set_permissions" | "set_mode" => {
            let line = crate::scan::line(m.span());
            if let Some(f) = s.current_fn() {
                f.chmods.push(line);
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
            bad.extend(exposed_reads(s, &tokens));
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
        let detail = if frame.chmods.iter().any(|c| c > line) {
            format!(
                "`{what}` creates a file in a module that writes credential-bearing documents and \
                 chmods it afterwards: until then it is readable under the umask. Give the mode at \
                 open (`OpenOptionsExt::mode(0o600)`)"
            )
        } else {
            format!(
                "`{what}` in a module that writes credential-bearing documents, with no owner-only mode set"
            )
        };
        s.out.push(crate::scan::Finding {
            axis: Axis::Security,
            file: s.file.to_string(),
            item: s.item(),
            line: *line,
            rule: "secret-doc-mode",
            detail,
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
