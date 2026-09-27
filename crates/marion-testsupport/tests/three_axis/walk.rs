//! Which files are marion's shipped code, and which are its tests.
//!
//! A text walk over `crates/*/src` cannot answer that: `src/pty/tests.rs` is test code reached
//! through `#[cfg(test)] mod tests;`, and `marion-provider` has a `src/` although nothing ships it.
//! So this follows `mod` declarations from each crate root the way rustc does, carrying "under
//! `#[cfg(test)]`" down the tree, and classifies whole crates by whether the shipped binary depends
//! on them.

use std::fs;
use std::path::{Path, PathBuf};

/// Crates whose `src/` is test infrastructure: only ever a `[dev-dependencies]` edge of the shipped
/// `marion-supervisor` (see its Cargo.toml). Their code is test code for every check here.
pub const TEST_SUPPORT_CRATES: &[&str] = &["marion-testsupport", "marion-provider"];

/// This suite's own source is the detectors' vocabulary — it spells `"login"`, `Harness::Codex`
/// and `thread::sleep` on purpose — so it is not scanned; `rules.rs` pins it instead.
const SELF: &str = "crates/marion-testsupport/tests/three_axis/";

pub struct Source {
    /// Workspace-relative, `/`-separated: the spelling allowlist keys use.
    pub rel: String,
    pub ast: syn::File,
    /// The whole file is test code (an integration test, a `#[cfg(test)] mod x;` target, or a
    /// test-support crate). Inside a production file, `#[cfg(test)]` items are still test code;
    /// the scanner tracks those itself.
    pub test_file: bool,
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

pub fn load_workspace(root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    let mut crate_dirs: Vec<PathBuf> = fs::read_dir(root.join("crates"))
        .expect("read crates/")
        .map(|e| e.expect("crates/ entry").path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    crate_dirs.sort();
    for dir in crate_dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let support = TEST_SUPPORT_CRATES.contains(&name.as_str());
        for crate_root in crate_roots(&dir) {
            walk_module_file(
                root,
                &crate_root,
                &dir_of_root(&crate_root),
                support,
                &mut out,
            );
        }
        for tests_dir in ["tests", "examples", "benches"] {
            for file in rust_files_under(&dir.join(tests_dir)) {
                push_parsed(root, &file, true, &mut out);
            }
        }
    }
    out.retain(|s| !s.rel.starts_with(SELF));
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.dedup_by(|a, b| a.rel == b.rel);
    out
}

/// `src/lib.rs`, `src/main.rs`, `src/bin/*.rs` and `src/bin/*/main.rs`: every target cargo builds
/// from `src/` in this workspace (the `[[bin]]` paths in the manifests all fall inside these).
fn crate_roots(dir: &Path) -> Vec<PathBuf> {
    let src = dir.join("src");
    let mut roots: Vec<PathBuf> = ["lib.rs", "main.rs"]
        .iter()
        .map(|f| src.join(f))
        .filter(|p| p.is_file())
        .collect();
    if let Ok(entries) = fs::read_dir(src.join("bin")) {
        let mut bins: Vec<PathBuf> = entries
            .map(|e| e.expect("bin entry").path())
            .filter_map(|p| {
                if p.extension().is_some_and(|e| e == "rs") {
                    Some(p)
                } else if p.join("main.rs").is_file() {
                    Some(p.join("main.rs"))
                } else {
                    None
                }
            })
            .collect();
        bins.sort();
        roots.extend(bins);
    }
    roots
}

fn dir_of_root(root_file: &Path) -> PathBuf {
    root_file.parent().unwrap().to_path_buf()
}

fn rust_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            out.extend(rust_files_under(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn push_parsed(root: &Path, file: &Path, test_file: bool, out: &mut Vec<Source>) -> syn::File {
    let text = fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let ast = syn::parse_file(&text).unwrap_or_else(|e| {
        panic!(
            "three-axis: {} does not parse as Rust ({e}); fix the syntax first",
            file.display()
        )
    });
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    out.push(Source {
        rel,
        ast: ast.clone(),
        test_file,
    });
    ast
}

/// Parse `file`, record it, and follow its out-of-line `mod x;` declarations. `mod_dir` is the
/// directory rustc resolves this file's child modules against.
fn walk_module_file(root: &Path, file: &Path, mod_dir: &Path, test: bool, out: &mut Vec<Source>) {
    let ast = push_parsed(root, file, test, out);
    let file_dir = file.parent().unwrap();
    walk_items(root, &ast.items, file_dir, mod_dir, test, out);
}

fn walk_items(
    root: &Path,
    items: &[syn::Item],
    file_dir: &Path,
    mod_dir: &Path,
    test: bool,
    out: &mut Vec<Source>,
) {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        let child_test = test || crate::scan::is_test_attrs(&m.attrs);
        let name = m.ident.to_string();
        match &m.content {
            Some((_, inner)) => {
                walk_items(root, inner, file_dir, &mod_dir.join(&name), child_test, out);
            }
            None => {
                let target = match path_attr(&m.attrs) {
                    Some(p) => file_dir.join(p),
                    None => {
                        let flat = mod_dir.join(format!("{name}.rs"));
                        if flat.is_file() {
                            flat
                        } else {
                            mod_dir.join(&name).join("mod.rs")
                        }
                    }
                };
                if !target.is_file() {
                    // A module behind a `#[cfg(...)]` for a platform whose file is absent, say.
                    continue;
                }
                let child_dir = if target.file_name().is_some_and(|f| f == "mod.rs") {
                    target.parent().unwrap().to_path_buf()
                } else {
                    target.with_extension("")
                };
                walk_module_file(root, &target, &child_dir, child_test, out);
            }
        }
    }
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| {
        let syn::Meta::NameValue(nv) = &a.meta else {
            return None;
        };
        if !nv.path.is_ident("path") {
            return None;
        }
        match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => Some(s.value()),
            _ => None,
        }
    })
}
