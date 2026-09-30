//! **`marion doctor --providers`**: what endpoint mode can do on this machine, without running a
//! node — each stored credential checked against its provider, and the harness × provider matrix.
//!
//! A credential row says whether its key is stored (never the key), the store file's mode where
//! the store is a file, and — where there is a key — whether the provider answers `GET
//! <base>/models` with it, in the header the provider row states, and whether a requested model is
//! listed. The request goes through `curl -sS` with the header on **stdin** (`-H @-`), so the key
//! reaches neither argv nor marion's own TLS stack; marion links none.
//!
//! The matrix is [`crate::endpoint`]'s own answer: the first wire of the harness row's recipes the
//! provider serves ([`crate::endpoint::shared_wire`]), and whether that recipe can present the
//! provider's key header — the two checks a launch makes, asked of every pair. Where no wire is
//! shared, a pair marion's gateway translates ([`crate::endpoint::translated_wire`]) is a
//! `translated` cell, spelled `<harness wire>><provider wire>`.

use std::io::Write;
use std::process::{Command, Stdio};

use marion_core::harness::Harness;
use marion_core::provider::{CredentialId, KeyHeader, ProviderDef, Registry, Wire};

use crate::credentials::CredentialStore;

/// How long one provider probe may take, connection included.
const PROBE_SECS: &str = "10";

/// What a provider said to a credential's `GET …/models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// A model listing answered; `model_listed` is whether the requested model is in it, where one
    /// was asked for.
    Listed {
        models: usize,
        model_listed: Option<bool>,
    },
    /// The provider refused the key (401 or 403).
    Refused(u16),
    /// Any other HTTP status.
    Status(u16),
    /// No answer at all: the words curl gave.
    Unreachable(String),
}

/// One stored credential, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRow {
    /// `provider` or `provider:label`.
    pub id: String,
    pub key_present: bool,
    /// The credential file's permission bits, where the store is a file that exists.
    pub file_mode: Option<u32>,
    /// `None` where nothing was probed: no key, or a provider marion does not know.
    pub reach: Option<Reach>,
}

/// One harness × provider pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixCell {
    pub harness: Harness,
    pub provider: String,
    pub cell: Cell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// The provider serves this wire of the harness's recipes natively.
    Native(String),
    /// No wire is shared, and marion's gateway translates the harness's wire to the provider's:
    /// `<harness wire>><provider wire>`, as `anthropic>openai-chat`.
    Translated(String),
    /// Why not, naming both wire lists or the key header.
    Unsupported(String),
}

/// The credential ids worth a row: the login index, every `[credentials]` order, and each
/// provider's unlabelled id wherever its key is stored — in that order, each once.
fn credential_ids(
    registry: &Registry,
    logins: &[CredentialId],
    store: &dyn CredentialStore,
) -> Vec<CredentialId> {
    let mut ids: Vec<CredentialId> = Vec::new();
    let mut push = |c: CredentialId| {
        if !ids.contains(&c) {
            ids.push(c);
        }
    };
    logins.iter().cloned().for_each(&mut push);
    for order in registry.credential_orders().values() {
        order.iter().cloned().for_each(&mut push);
    }
    for p in registry.iter().filter(|p| p.auth.needs_credential()) {
        let c = CredentialId::default_for(&p.id);
        if p.custom || store.get(&c.to_string()).ok().flatten().is_some() {
            push(c);
        }
    }
    ids
}

/// The wire a provider's listing is asked on: an OpenAI wire where it serves one (the listing
/// every compatible server answers), else its first.
fn probe_wire(p: &ProviderDef) -> Option<(Wire, &str)> {
    [Wire::OpenAiChat, Wire::OpenAiResponses]
        .into_iter()
        .find_map(|w| p.base_for(w).map(|b| (w, b)))
        .or_else(|| p.wires.first().map(|(w, b)| (*w, b.as_str())))
}

/// `GET <base>/models` on `wire`, and the headers that carry `key` there: the provider's own key
/// header on the Anthropic and OpenAI wires (plus the version header Anthropic's API requires),
/// `x-goog-api-key` on Gemini's.
fn models_request(wire: Wire, base: &str, header: KeyHeader, key: &str) -> (String, String) {
    let base = base.trim_end_matches('/');
    let key_line = match header {
        KeyHeader::Bearer => format!("Authorization: Bearer {key}"),
        KeyHeader::XApiKey => format!("x-api-key: {key}"),
    };
    match wire {
        Wire::OpenAiChat | Wire::OpenAiResponses => (format!("{base}/models"), key_line),
        Wire::AnthropicMessages => (
            format!("{base}/v1/models"),
            format!("{key_line}\nanthropic-version: 2023-06-01"),
        ),
        Wire::GenerateContent => (
            format!("{base}/v1beta/models"),
            format!("x-goog-api-key: {key}"),
        ),
    }
}

/// Run curl for `url` with `headers` on its stdin; the status and body, or curl's own words.
fn curl_get(url: &str, headers: &str) -> Result<(u16, String), String> {
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "-m",
            PROBE_SECS,
            "-H",
            "@-",
            "-w",
            "\n%{http_code}",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{headers}\n").as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("curl did not finish: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body, code) = stdout.rsplit_once('\n').unwrap_or(("", stdout.as_ref()));
    match code.trim().parse::<u16>() {
        Ok(c) if c != 0 => Ok((c, body.to_string())),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// The model ids a listing names: OpenAI's and Anthropic's `data[].id`, Gemini's `models[].name`
/// without its `models/` prefix.
fn listed_models(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let ids = |key: &str, field: &str| -> Option<Vec<String>> {
        v.get(key)?.as_array().map(|a| {
            a.iter()
                .filter_map(|m| m.get(field)?.as_str())
                .map(|s| s.trim_start_matches("models/").to_string())
                .collect()
        })
    };
    ids("data", "id").or_else(|| ids("models", "name"))
}

fn probe(p: &ProviderDef, key: &str, model: Option<&str>) -> Option<Reach> {
    let (wire, base) = probe_wire(p)?;
    let (url, headers) = models_request(wire, base, p.key_header, key);
    Some(match curl_get(&url, &headers) {
        Err(why) => Reach::Unreachable(crate::endpoint::redact(&why, key)),
        Ok((code @ (401 | 403), _)) => Reach::Refused(code),
        Ok((200, body)) => match listed_models(&body) {
            Some(ids) => Reach::Listed {
                models: ids.len(),
                model_listed: model.map(|m| ids.iter().any(|i| i == m)),
            },
            None => Reach::Status(200),
        },
        Ok((code, _)) => Reach::Status(code),
    })
}

/// Check every credential worth a row against `registry` and `store`. `model` is a model id to
/// look for in each listing, with any `<provider>:` prefix of that provider removed.
pub fn check(
    registry: &Registry,
    logins: &[CredentialId],
    store: &dyn CredentialStore,
    file: Option<&std::path::Path>,
    model: Option<&str>,
) -> Vec<CredentialRow> {
    let file_mode = file.and_then(|f| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(f)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    });
    credential_ids(registry, logins, store)
        .into_iter()
        .map(|c| {
            let key = store.get(&c.to_string()).ok().flatten();
            let wanted = model.map(|m| {
                m.strip_prefix(&format!("{}:", c.provider))
                    .unwrap_or(m)
                    .to_string()
            });
            let reach = match (registry.get(&c.provider), &key) {
                (Some(p), Some(k)) => probe(p, k.expose(), wanted.as_deref()),
                _ => None,
            };
            CredentialRow {
                id: c.to_string(),
                key_present: key.is_some(),
                file_mode,
                reach,
            }
        })
        .collect()
}

/// [`check`] against the user's own registry, login index and store.
pub fn check_user(model: Option<&str>) -> Result<Vec<CredentialRow>, String> {
    let registry = crate::credentials::user_registry()?;
    let store = crate::credentials::default_store().map_err(|e| e.to_string())?;
    let logins = crate::credentials::Logins::user()
        .and_then(|l| l.all())
        .map_err(|e| e.to_string())?;
    let file = store
        .describe()
        .strip_prefix("file ")
        .map(std::path::PathBuf::from);
    Ok(check(
        &registry,
        &logins,
        store.as_ref(),
        file.as_deref(),
        model,
    ))
}

/// Every harness × provider pair, as a launch would resolve it.
pub fn matrix(registry: &Registry) -> Vec<MatrixCell> {
    let mut out = Vec::new();
    for h in Harness::ALL {
        let spec = marion_harness::adapter::harness_spec(h);
        let wires: Vec<Wire> = spec.wires.iter().map(|r| r.wire).collect();
        for p in registry.iter() {
            let cell = match crate::endpoint::shared_wire(&wires, p) {
                // The gateway reads a Bearer credential whatever the provider reads, so the
                // harness's recipe must present that one.
                None => match crate::endpoint::translated_wire(&wires, p) {
                    Some((hw, pw, _))
                        if marion_harness::spec::recipe_for(spec, Some(hw)).is_some_and(|r| {
                            r.keys.iter().any(|k| k.header == KeyHeader::Bearer)
                        }) =>
                    {
                        Cell::Translated(format!("{hw}>{pw}"))
                    }
                    _ => Cell::Unsupported(format!(
                        "{h} speaks {} and {} serves {}",
                        crate::endpoint::wire_list(&wires),
                        p.id,
                        p.wire_list()
                    )),
                },
                Some((w, _)) => {
                    let presents = marion_harness::spec::recipe_for(spec, Some(w))
                        .is_some_and(|r| r.keys.iter().any(|k| k.header == p.key_header));
                    if presents {
                        Cell::Native(w.as_str().to_string())
                    } else {
                        Cell::Unsupported(format!(
                            "{h}'s {w} recipe cannot present a key in `{}`",
                            p.key_header.as_str()
                        ))
                    }
                }
            };
            out.push(MatrixCell {
                harness: h,
                provider: p.id.clone(),
                cell,
            });
        }
    }
    out
}

fn reach_text(r: &Option<Reach>) -> String {
    match r {
        None => "not probed".into(),
        Some(Reach::Listed {
            models,
            model_listed,
        }) => {
            let asked = match model_listed {
                Some(true) => ", requested model listed",
                Some(false) => ", requested model NOT listed",
                None => "",
            };
            format!("reachable, {models} models listed{asked}")
        }
        Some(Reach::Refused(c)) => format!("key refused ({c})"),
        Some(Reach::Status(c)) => format!("answered {c}"),
        Some(Reach::Unreachable(why)) => format!("unreachable: {why}"),
    }
}

/// The report: one line per credential, then the matrix by provider, then why each unsupported
/// pair is unsupported.
pub fn render(rows: &[CredentialRow], matrix: &[MatrixCell]) -> String {
    let mut out = String::from("credentials:\n");
    if rows.is_empty() {
        out.push_str("  none stored; `marion key add <provider>` stores one\n");
    }
    for r in rows {
        let key = if r.key_present {
            "key stored"
        } else {
            "no key"
        };
        let mode = r
            .file_mode
            .map(|m| format!(", file mode {m:o}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {}: {key}{mode}, {}\n",
            r.id,
            reach_text(&r.reach)
        ));
    }
    out.push_str("\nharness x provider (native wire, or -):\n");
    let mut providers: Vec<&str> = Vec::new();
    for c in matrix {
        if !providers.contains(&c.provider.as_str()) {
            providers.push(&c.provider);
        }
    }
    for p in &providers {
        let cells: Vec<String> = Harness::ALL
            .into_iter()
            .filter_map(|h| {
                matrix
                    .iter()
                    .find(|c| c.harness == h && c.provider == *p)
                    .map(|c| match &c.cell {
                        Cell::Native(w) | Cell::Translated(w) => format!("{}={w}", h.cli_name()),
                        Cell::Unsupported(_) => format!("{}=-", h.cli_name()),
                    })
            })
            .collect();
        out.push_str(&format!("  {p}: {}\n", cells.join(" ")));
    }
    out.push_str("\nunsupported:\n");
    for c in matrix {
        if let Cell::Unsupported(why) = &c.cell {
            out.push_str(&format!(
                "  {} on {}: {why}\n",
                c.harness.cli_name(),
                c.provider
            ));
        }
    }
    out
}

/// `doctor --providers [--model <id>]`: `Some(model)` where `argv` asks for the providers check,
/// `None` where it does not; any other flag beside `--providers` is an error, as doctor's are.
pub fn parse_args(argv: &[String]) -> Option<Result<Option<String>, String>> {
    if !argv.iter().any(|a| a == "--providers") {
        return None;
    }
    let mut model = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--providers" => {}
            "--model" => match it.next() {
                Some(m) => model = Some(m.clone()),
                None => return Some(Err("--model needs a model id".into())),
            },
            other => {
                return Some(Err(format!(
                    "`{other}` does not combine with --providers (only --model does)"
                )));
            }
        }
    }
    Some(Ok(model))
}

/// The whole `doctor --providers` report for this user.
pub fn report(model: Option<&str>) -> Result<String, String> {
    let rows = check_user(model)?;
    let registry = crate::credentials::user_registry()?;
    Ok(render(&rows, &matrix(&registry)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn providers_takes_a_model_and_nothing_else() {
        assert_eq!(parse_args(&argv(&["--capabilities"])), None);
        assert_eq!(parse_args(&argv(&["--providers"])), Some(Ok(None)));
        assert_eq!(
            parse_args(&argv(&["--providers", "--model", "m-1"])),
            Some(Ok(Some("m-1".into())))
        );
        assert!(matches!(
            parse_args(&argv(&["--providers", "--adapter"])),
            Some(Err(_))
        ));
    }

    #[test]
    fn a_listing_is_read_in_each_vendors_shape() {
        assert_eq!(
            listed_models(r#"{"object":"list","data":[{"id":"a"},{"id":"b"}]}"#),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            listed_models(r#"{"models":[{"name":"models/gemini-2.5-pro"}]}"#),
            Some(vec!["gemini-2.5-pro".to_string()])
        );
        assert_eq!(listed_models("marion canned provider\n"), None);
    }

    #[test]
    fn each_wire_is_asked_at_its_listing_path_with_the_key_in_the_providers_header() {
        let (u, h) = models_request(
            Wire::AnthropicMessages,
            "https://api.anthropic.com",
            KeyHeader::XApiKey,
            "sk-x",
        );
        assert_eq!(u, "https://api.anthropic.com/v1/models");
        assert!(h.starts_with("x-api-key: sk-x\n") && h.contains("anthropic-version"));
        let (u, h) = models_request(
            Wire::OpenAiChat,
            "https://api.groq.com/openai/v1/",
            KeyHeader::Bearer,
            "gsk",
        );
        assert_eq!(
            (u.as_str(), h.as_str()),
            (
                "https://api.groq.com/openai/v1/models",
                "Authorization: Bearer gsk"
            )
        );
        let (u, h) = models_request(
            Wire::GenerateContent,
            "https://g.example",
            KeyHeader::Bearer,
            "k",
        );
        assert_eq!(
            (u.as_str(), h.as_str()),
            ("https://g.example/v1beta/models", "x-goog-api-key: k")
        );
    }
}
