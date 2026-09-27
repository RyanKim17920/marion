//! The provider registry: **which model endpoints marion can point a node at, as data.**
//!
//! Endpoint mode lets any harness talk to any model the user holds a key for. A provider is a row:
//! the wires it serves natively, where each one lives, and how it is authenticated. The seed table
//! below ships with marion; a user adds their own in `$XDG_CONFIG_HOME/marion/providers.toml`
//! ([`Registry::with_custom`] takes that file's **text** — the supervisor opens it, so this crate
//! stays free of I/O, as `agent_type` is).
//!
//! # What a wire base means
//!
//! Each `(Wire, base)` pair is the base URL a client of that wire is handed: the `…/v1`-style
//! prefix for the two OpenAI wires, the root (no `/v1`) for Anthropic Messages and Gemini — the
//! spelling each vendor's own SDK takes. The adapters already derive their harness's spelling from
//! a launch's base URL (`claude_code::anthropic_base_url` strips a trailing `/v1`,
//! `gemini::google_base_url` likewise), so either form reaches the harness correctly.
//!
//! # What is and is not verified
//!
//! The seed rows are transcribed from each vendor's published documentation, **not measured**:
//! marion's E2E matrix proves a wire against its own canned endpoint (a `providers.toml` entry
//! pointing at it), never against a vendor. A wire whose compatibility layer the vendor documents
//! as beta or which marion has not seen documented at all is marked *unverified* on its row.
//!
//! # Out of scope, permanently
//!
//! No row reuses a vendor **subscription** login (a Claude, ChatGPT, Copilot or Google account)
//! outside that vendor's own harness, and none spoofs a client identity. [`AuthKind::OAuthPkce`]
//! exists for providers that publish a third-party OAuth flow for exactly this use (OpenRouter's);
//! no seed row uses it yet.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A request/response wire format a harness can speak and a provider can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Wire {
    /// Anthropic Messages (`POST /v1/messages`).
    #[serde(rename = "anthropic")]
    AnthropicMessages,
    /// OpenAI Chat Completions (`POST /chat/completions`).
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    /// OpenAI Responses (`POST /responses`).
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// Google Gemini `generateContent`.
    #[serde(rename = "gemini")]
    Gemini,
}

impl Wire {
    pub const ALL: [Wire; 4] = [
        Wire::AnthropicMessages,
        Wire::OpenAiChat,
        Wire::OpenAiResponses,
        Wire::Gemini,
    ];

    /// The spelling a user types (`--wire`, `providers.toml`) and a record carries.
    pub const fn as_str(self) -> &'static str {
        match self {
            Wire::AnthropicMessages => "anthropic",
            Wire::OpenAiChat => "openai-chat",
            Wire::OpenAiResponses => "openai-responses",
            Wire::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Option<Wire> {
        Wire::ALL.into_iter().find(|w| w.as_str() == s.trim())
    }
}

impl fmt::Display for Wire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a provider is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// A key the user pastes into `marion login <provider>`.
    ApiKey,
    /// A provider-published third-party OAuth flow (authorization-code + PKCE). No seed row uses
    /// it yet; OpenRouter's flow is a later phase.
    OAuthPkce {
        authorize: &'static str,
        exchange: &'static str,
    },
    /// A local server that authenticates nothing (ollama, LM Studio). Needs no `marion login`.
    None,
}

impl AuthKind {
    /// Whether a node on this provider needs a stored credential.
    pub const fn needs_credential(self) -> bool {
        !matches!(self, AuthKind::None)
    }
}

/// **Which header a provider reads its key from** on the Anthropic and OpenAI wires. A column of
/// the row rather than a branch on its name: Anthropic's own API takes `x-api-key` (what its SDKs
/// send for an API key), the gateways that serve Anthropic Messages document `Authorization:
/// Bearer`, and a harness recipe renders whichever the row states. Gemini's wire carries its key in
/// its own header whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyHeader {
    /// `Authorization: Bearer <key>` — every OpenAI-compatible server, and the default.
    #[default]
    Bearer,
    /// `x-api-key: <key>`.
    XApiKey,
}

impl KeyHeader {
    /// The spelling `providers.toml` takes.
    pub const fn as_str(self) -> &'static str {
        match self {
            KeyHeader::Bearer => "bearer",
            KeyHeader::XApiKey => "x-api-key",
        }
    }

    pub fn parse(s: &str) -> Option<KeyHeader> {
        [KeyHeader::Bearer, KeyHeader::XApiKey]
            .into_iter()
            .find(|h| h.as_str() == s.trim())
    }
}

/// One seed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// The registry id: what `marion login <id>`, an agent type's `provider` and a `<id>:<model>`
    /// prefix name.
    pub id: &'static str,
    pub name: &'static str,
    /// The provider's API root, for display.
    pub base_url: &'static str,
    /// The wires the provider serves **natively**, each with its base, in the provider's own order
    /// of preference.
    pub wires: &'static [(Wire, &'static str)],
    pub auth: AuthKind,
    /// The environment variable the provider's own tooling reads its key from. `marion login`
    /// offers to import it, with consent; marion never reads it on its own.
    pub import_env: Option<&'static str>,
    /// Whether the provider answers a model listing (`GET …/models`). For a later `doctor
    /// --providers`; nothing reads it yet.
    pub list_models: bool,
    /// The header the key rides ([`KeyHeader`]).
    pub key_header: KeyHeader,
}

const API: AuthKind = AuthKind::ApiKey;

/// The providers marion knows without configuration.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        wires: &[(Wire::AnthropicMessages, "https://api.anthropic.com")],
        auth: API,
        import_env: Some("ANTHROPIC_API_KEY"),
        list_models: true,
        // What Anthropic's SDKs send for an API key; the API's docs also accept a Bearer key and
        // call `x-api-key` the legacy fallback, still supported.
        key_header: KeyHeader::XApiKey,
    },
    Provider {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        wires: &[
            (Wire::OpenAiResponses, "https://api.openai.com/v1"),
            (Wire::OpenAiChat, "https://api.openai.com/v1"),
        ],
        auth: API,
        import_env: Some("OPENAI_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // Anthropic Messages at `/api` is OpenRouter's documented Claude Code route. Its OAuth PKCE
    // flow is a later phase; a pasted key works today.
    Provider {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        wires: &[
            (Wire::OpenAiChat, "https://openrouter.ai/api/v1"),
            (Wire::AnthropicMessages, "https://openrouter.ai/api"),
        ],
        auth: API,
        import_env: Some("OPENROUTER_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // Google AI Studio keys. The OpenAI-compatible layer at `/v1beta/openai` is documented as
    // beta: unverified.
    Provider {
        id: "gemini",
        name: "Google Gemini (AI Studio)",
        base_url: "https://generativelanguage.googleapis.com",
        wires: &[
            (Wire::Gemini, "https://generativelanguage.googleapis.com"),
            (
                Wire::OpenAiChat,
                "https://generativelanguage.googleapis.com/v1beta/openai",
            ),
        ],
        auth: API,
        import_env: Some("GEMINI_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // Anthropic Messages at `/anthropic`: documented for Claude Code, unverified by marion.
    Provider {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        wires: &[
            (Wire::OpenAiChat, "https://api.deepseek.com/v1"),
            (
                Wire::AnthropicMessages,
                "https://api.deepseek.com/anthropic",
            ),
        ],
        auth: API,
        import_env: Some("DEEPSEEK_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // Anthropic Messages at `/anthropic`: documented for Claude Code, unverified by marion.
    Provider {
        id: "moonshot",
        name: "Moonshot AI (Kimi)",
        base_url: "https://api.moonshot.ai",
        wires: &[
            (Wire::OpenAiChat, "https://api.moonshot.ai/v1"),
            (Wire::AnthropicMessages, "https://api.moonshot.ai/anthropic"),
        ],
        auth: API,
        import_env: Some("MOONSHOT_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // Anthropic Messages at `/api/anthropic`: documented for Claude Code, unverified by marion.
    Provider {
        id: "zai",
        name: "Z.ai (GLM)",
        base_url: "https://api.z.ai/api",
        wires: &[
            (Wire::OpenAiChat, "https://api.z.ai/api/paas/v4"),
            (Wire::AnthropicMessages, "https://api.z.ai/api/anthropic"),
        ],
        auth: API,
        import_env: Some("ZAI_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    Provider {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        wires: &[(Wire::OpenAiChat, "https://api.groq.com/openai/v1")],
        auth: API,
        import_env: Some("GROQ_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    Provider {
        id: "together",
        name: "Together AI",
        base_url: "https://api.together.xyz/v1",
        wires: &[(Wire::OpenAiChat, "https://api.together.xyz/v1")],
        auth: API,
        import_env: Some("TOGETHER_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    Provider {
        id: "fireworks",
        name: "Fireworks AI",
        base_url: "https://api.fireworks.ai/inference/v1",
        wires: &[(Wire::OpenAiChat, "https://api.fireworks.ai/inference/v1")],
        auth: API,
        import_env: Some("FIREWORKS_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    Provider {
        id: "mistral",
        name: "Mistral AI",
        base_url: "https://api.mistral.ai/v1",
        wires: &[(Wire::OpenAiChat, "https://api.mistral.ai/v1")],
        auth: API,
        import_env: Some("MISTRAL_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // OpenAI Responses on xAI: documented, unverified by marion.
    Provider {
        id: "xai",
        name: "xAI (Grok)",
        base_url: "https://api.x.ai/v1",
        wires: &[
            (Wire::OpenAiChat, "https://api.x.ai/v1"),
            (Wire::OpenAiResponses, "https://api.x.ai/v1"),
        ],
        auth: API,
        import_env: Some("XAI_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    // Responses and Anthropic Messages on the gateway: documented, unverified by marion.
    Provider {
        id: "vercel-gateway",
        name: "Vercel AI Gateway",
        base_url: "https://ai-gateway.vercel.sh/v1",
        wires: &[
            (Wire::OpenAiChat, "https://ai-gateway.vercel.sh/v1"),
            (Wire::OpenAiResponses, "https://ai-gateway.vercel.sh/v1"),
            (Wire::AnthropicMessages, "https://ai-gateway.vercel.sh"),
        ],
        auth: API,
        import_env: Some("AI_GATEWAY_API_KEY"),
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    Provider {
        id: "ollama",
        name: "Ollama (local)",
        base_url: "http://127.0.0.1:11434",
        wires: &[(Wire::OpenAiChat, "http://127.0.0.1:11434/v1")],
        auth: AuthKind::None,
        import_env: None,
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
    Provider {
        id: "lmstudio",
        name: "LM Studio (local)",
        base_url: "http://127.0.0.1:1234/v1",
        wires: &[(Wire::OpenAiChat, "http://127.0.0.1:1234/v1")],
        auth: AuthKind::None,
        import_env: None,
        list_models: true,
        key_header: KeyHeader::Bearer,
    },
];

/// A provider as the registry answers it: a seed row, or a user's `providers.toml` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDef {
    pub id: String,
    pub name: String,
    pub wires: Vec<(Wire, String)>,
    pub auth: AuthKind,
    pub import_env: Option<String>,
    /// The header the key rides ([`KeyHeader`]).
    pub key_header: KeyHeader,
    /// From the user's `providers.toml`, not the seed table.
    pub custom: bool,
}

impl ProviderDef {
    /// The base this provider serves `wire` at, or `None` where it does not serve it natively.
    pub fn base_for(&self, wire: Wire) -> Option<&str> {
        self.wires
            .iter()
            .find(|(w, _)| *w == wire)
            .map(|(_, b)| b.as_str())
    }

    /// The wires, comma-joined in the provider's order, for display and refusals.
    pub fn wire_list(&self) -> String {
        self.wires
            .iter()
            .map(|(w, _)| w.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl From<&Provider> for ProviderDef {
    fn from(p: &Provider) -> Self {
        ProviderDef {
            id: p.id.to_string(),
            name: p.name.to_string(),
            wires: p.wires.iter().map(|(w, b)| (*w, b.to_string())).collect(),
            auth: p.auth,
            import_env: p.import_env.map(str::to_string),
            key_header: p.key_header,
            custom: false,
        }
    }
}

/// Why a `providers.toml` or a custom provider definition was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("providers.toml does not parse: {0}")]
    Parse(String),
    #[error(
        "provider id `{0}` is not valid: use lowercase letters, digits and `-`, starting with a \
         letter or digit"
    )]
    BadId(String),
    #[error("provider `{0}` is built in; pick another id for a custom provider")]
    ShadowsSeed(String),
    #[error("provider `{id}`: base_url `{url}` must start with http:// or https://")]
    BadUrl { id: String, url: String },
    #[error(
        "provider `{id}`: unknown wire `{wire}` (known: anthropic, openai-chat, openai-responses, gemini)"
    )]
    UnknownWire { id: String, wire: String },
    #[error("provider `{0}` declares no wires")]
    NoWires(String),
    #[error("provider `{id}`: auth `{auth}` is not one of api-key, none")]
    BadAuth { id: String, auth: String },
    #[error("provider `{id}`: key_header `{header}` is not one of bearer, x-api-key")]
    BadKeyHeader { id: String, header: String },
    #[error(
        "[credentials] {provider}: `{credential}` is not a credential id for that provider \
         (`{provider}` or `{provider}:<label>`)"
    )]
    BadCredential {
        provider: String,
        credential: String,
    },
    #[error("[credentials] names `{0}`, which is neither built in nor defined in this file")]
    UnknownCredentialProvider(String),
}

/// One `[providers.<id>]` table, as written.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCustom {
    #[serde(default)]
    name: Option<String>,
    base_url: String,
    wires: Vec<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    import_env: Option<String>,
    #[serde(default)]
    key_header: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    providers: BTreeMap<String, RawCustom>,
    /// `[credentials]`: per provider, the ordered credential ids a launch tries — the first with a
    /// stored key is used. A provider not named here uses its credentials in login order.
    #[serde(default)]
    credentials: BTreeMap<String, Vec<String>>,
}

/// **One stored credential**: a provider id and an optional label — `openrouter` or
/// `openrouter:work`. A user may hold several credentials for one provider (a work and a personal
/// key); the label tells them apart. Both halves take [`valid_id`]'s grammar, so an id is safe as a
/// Keychain account, a JSON key and a record field without escaping.
///
/// **API keys and provider-published OAuth only.** This is never a way to hold several vendor
/// *subscription* logins for one harness and switch between them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialId {
    pub provider: String,
    pub label: Option<String>,
}

impl CredentialId {
    /// `provider` or `provider:label`, each half a [`valid_id`].
    pub fn parse(s: &str) -> Option<Self> {
        let (provider, label) = match s.split_once(':') {
            Some((p, l)) => (p, Some(l)),
            None => (s, None),
        };
        if !valid_id(provider) || label.is_some_and(|l| !valid_id(l)) {
            return None;
        }
        Some(CredentialId {
            provider: provider.to_string(),
            label: label.map(str::to_string),
        })
    }

    /// The provider's unlabeled credential — the one `marion login <provider>` stores.
    pub fn default_for(provider: &str) -> Self {
        CredentialId {
            provider: provider.to_string(),
            label: None,
        }
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.label {
            Some(l) => write!(f, "{}:{l}", self.provider),
            None => f.write_str(&self.provider),
        }
    }
}

/// Whether `id` is a spelling a custom provider may take.
pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A validated custom provider: the one constructor both the file and `marion login custom` go
/// through, so the two cannot accept different things.
pub fn custom_provider(
    id: &str,
    base_url: &str,
    wires: &[Wire],
    auth: AuthKind,
    name: Option<&str>,
    import_env: Option<&str>,
) -> Result<ProviderDef, ProviderError> {
    if !valid_id(id) {
        return Err(ProviderError::BadId(id.to_string()));
    }
    if PROVIDERS.iter().any(|p| p.id == id) {
        return Err(ProviderError::ShadowsSeed(id.to_string()));
    }
    let url = base_url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ProviderError::BadUrl {
            id: id.to_string(),
            url: base_url.to_string(),
        });
    }
    if wires.is_empty() {
        return Err(ProviderError::NoWires(id.to_string()));
    }
    let mut seen = Vec::new();
    for w in wires {
        if !seen.contains(w) {
            seen.push(*w);
        }
    }
    Ok(ProviderDef {
        id: id.to_string(),
        name: name.unwrap_or(id).to_string(),
        wires: seen.into_iter().map(|w| (w, url.to_string())).collect(),
        auth,
        import_env: import_env.map(str::to_string),
        key_header: KeyHeader::Bearer,
        custom: true,
    })
}

/// The registry a launch resolves against: the seed table plus the user's custom providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    defs: Vec<ProviderDef>,
    /// `[credentials]`: the user's stated credential order, per provider id.
    order: BTreeMap<String, Vec<CredentialId>>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::seed()
    }
}

impl Registry {
    /// The seed table alone.
    pub fn seed() -> Self {
        Registry {
            defs: PROVIDERS.iter().map(ProviderDef::from).collect(),
            order: BTreeMap::new(),
        }
    }

    /// The seed table plus the custom providers in a `providers.toml`'s text.
    pub fn with_custom(text: &str) -> Result<Self, ProviderError> {
        let mut reg = Registry::seed();
        reg.defs.extend(parse_custom(text)?);
        let raw: RawFile = toml::from_str(text).map_err(|e| ProviderError::Parse(e.to_string()))?;
        for (provider, ids) in raw.credentials {
            if reg.get(&provider).is_none() {
                return Err(ProviderError::UnknownCredentialProvider(provider));
            }
            let mut order = Vec::new();
            for id in ids {
                match CredentialId::parse(&id) {
                    Some(c) if c.provider == provider => order.push(c),
                    _ => {
                        return Err(ProviderError::BadCredential {
                            provider,
                            credential: id,
                        });
                    }
                }
            }
            reg.order.insert(provider, order);
        }
        Ok(reg)
    }

    /// Every `[credentials]` order the user stated, by provider — what a rewrite of the file must
    /// carry forward.
    pub fn credential_orders(&self) -> &BTreeMap<String, Vec<CredentialId>> {
        &self.order
    }

    /// The credential order the user stated for `provider` in `[credentials]`, if they stated one.
    pub fn credential_order(&self, provider: &str) -> Option<&[CredentialId]> {
        self.order.get(provider).map(Vec::as_slice)
    }

    pub fn get(&self, id: &str) -> Option<&ProviderDef> {
        self.defs.iter().find(|d| d.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ProviderDef> {
        self.defs.iter()
    }

    /// The user's custom providers alone, in file order.
    pub fn custom(&self) -> impl Iterator<Item = &ProviderDef> {
        self.defs.iter().filter(|d| d.custom)
    }

    /// Split a `<provider>:<model>` request — on the **first** colon only, and only when what is
    /// before it is a registry id, so `ollama:qwen3:32b` is ollama's `qwen3:32b` and a bare
    /// `qwen3:32b` stays a model id.
    pub fn split_model<'m>(&self, model: &'m str) -> Option<(&ProviderDef, &'m str)> {
        let (prefix, rest) = model.split_once(':')?;
        if rest.is_empty() {
            return None;
        }
        self.get(prefix).map(|p| (p, rest))
    }
}

/// The custom providers in a `providers.toml`'s text, validated.
pub fn parse_custom(text: &str) -> Result<Vec<ProviderDef>, ProviderError> {
    let raw: RawFile = toml::from_str(text).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    for (id, row) in raw.providers {
        let mut wires = Vec::new();
        for w in &row.wires {
            wires.push(Wire::parse(w).ok_or_else(|| ProviderError::UnknownWire {
                id: id.clone(),
                wire: w.clone(),
            })?);
        }
        let auth = match row.auth.as_deref().map(str::trim) {
            None | Some("api-key") => AuthKind::ApiKey,
            Some("none") => AuthKind::None,
            Some(other) => {
                return Err(ProviderError::BadAuth {
                    id,
                    auth: other.to_string(),
                });
            }
        };
        let key_header = match row.key_header.as_deref() {
            None => KeyHeader::Bearer,
            Some(h) => KeyHeader::parse(h).ok_or_else(|| ProviderError::BadKeyHeader {
                id: id.clone(),
                header: h.to_string(),
            })?,
        };
        let mut def = custom_provider(
            &id,
            &row.base_url,
            &wires,
            auth,
            row.name.as_deref(),
            row.import_env.as_deref(),
        )?;
        def.key_header = key_header;
        out.push(def);
    }
    Ok(out)
}

/// A TOML basic string, escaped.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A `providers.toml` holding exactly these custom providers and credential orders — what `marion
/// login custom` writes back after adding or replacing one. Comments in the old file are not
/// preserved; the header says so.
pub fn render_custom(defs: &[ProviderDef], order: &BTreeMap<String, Vec<CredentialId>>) -> String {
    let mut out = String::from(
        "# marion's custom providers. Rewritten by `marion login custom`; comments are not kept.\n",
    );
    for d in defs.iter().filter(|d| d.custom) {
        let base = d.wires.first().map(|(_, b)| b.as_str()).unwrap_or("");
        let wires = d
            .wires
            .iter()
            .map(|(w, _)| toml_str(w.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("\n[providers.{}]\n", d.id));
        if d.name != d.id {
            out.push_str(&format!("name = {}\n", toml_str(&d.name)));
        }
        out.push_str(&format!("base_url = {}\n", toml_str(base)));
        out.push_str(&format!("wires = [{wires}]\n"));
        if d.auth == AuthKind::None {
            out.push_str("auth = \"none\"\n");
        }
        if let Some(e) = &d.import_env {
            out.push_str(&format!("import_env = {}\n", toml_str(e)));
        }
        if d.key_header != KeyHeader::Bearer {
            out.push_str(&format!(
                "key_header = {}\n",
                toml_str(d.key_header.as_str())
            ));
        }
    }
    if !order.is_empty() {
        out.push_str("\n[credentials]\n");
        for (provider, ids) in order {
            let ids = ids
                .iter()
                .map(|c| toml_str(&c.to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("{provider} = [{ids}]\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_seed_row_has_a_unique_valid_id_and_at_least_one_wire() {
        let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        for p in PROVIDERS {
            assert!(valid_id(p.id), "{}", p.id);
            assert!(!p.wires.is_empty(), "{} serves no wire", p.id);
            for (_, base) in p.wires {
                assert!(
                    base.starts_with("https://") || base.starts_with("http://127.0.0.1"),
                    "{}: {base}",
                    p.id
                );
            }
        }
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate provider id");
    }

    /// **Every seed row states its wires and its auth as data**: no wire twice, a base of the
    /// shape its wire's clients take (no trailing slash), and — where it needs a credential — the
    /// environment variable its own tooling reads, which `marion login --from-env` imports.
    #[test]
    fn every_seed_row_states_distinct_wires_and_how_it_is_authenticated() {
        for p in PROVIDERS {
            let mut seen = Vec::new();
            for (w, base) in p.wires {
                assert!(!seen.contains(w), "{}: {w} twice", p.id);
                seen.push(*w);
                assert!(!base.ends_with('/'), "{}: {base}", p.id);
            }
            match p.auth {
                AuthKind::ApiKey => assert!(p.import_env.is_some(), "{}: no import_env", p.id),
                AuthKind::None => {
                    assert!(p.import_env.is_none(), "{}", p.id);
                    assert!(
                        p.base_url.starts_with("http://127.0.0.1"),
                        "{}: a key-less \
                             provider is a local server",
                        p.id
                    );
                }
                AuthKind::OAuthPkce { .. } => panic!("{}: no seed row uses PKCE yet", p.id),
            }
        }
    }

    #[test]
    fn the_seed_table_names_every_provider_the_plan_promised() {
        let reg = Registry::seed();
        for id in [
            "anthropic",
            "openai",
            "openrouter",
            "gemini",
            "deepseek",
            "moonshot",
            "zai",
            "groq",
            "together",
            "fireworks",
            "mistral",
            "xai",
            "vercel-gateway",
            "ollama",
            "lmstudio",
        ] {
            assert!(reg.get(id).is_some(), "{id} is missing");
        }
    }

    #[test]
    fn openrouter_serves_chat_and_anthropic_at_its_documented_bases() {
        let reg = Registry::seed();
        let or = reg.get("openrouter").unwrap();
        assert_eq!(
            or.base_for(Wire::OpenAiChat),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(
            or.base_for(Wire::AnthropicMessages),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(or.base_for(Wire::Gemini), None);
        assert_eq!(or.auth, AuthKind::ApiKey);
    }

    #[test]
    fn local_servers_need_no_credential() {
        let reg = Registry::seed();
        for id in ["ollama", "lmstudio"] {
            assert!(!reg.get(id).unwrap().auth.needs_credential(), "{id}");
        }
        assert!(reg.get("openai").unwrap().auth.needs_credential());
    }

    #[test]
    fn a_model_prefix_splits_on_the_first_colon_only_and_only_for_a_registry_id() {
        let reg = Registry::seed();
        let (p, m) = reg.split_model("ollama:qwen3:32b").unwrap();
        assert_eq!((p.id.as_str(), m), ("ollama", "qwen3:32b"));
        let (p, m) = reg
            .split_model("openrouter:anthropic/claude-sonnet-4")
            .unwrap();
        assert_eq!(
            (p.id.as_str(), m),
            ("openrouter", "anthropic/claude-sonnet-4")
        );
        assert!(
            reg.split_model("qwen3:32b").is_none(),
            "qwen3 is no provider"
        );
        assert!(reg.split_model("gpt-5").is_none());
        assert!(
            reg.split_model("openai:").is_none(),
            "an empty model is no split"
        );
    }

    #[test]
    fn wires_round_trip_through_their_spelling() {
        for w in Wire::ALL {
            assert_eq!(Wire::parse(w.as_str()), Some(w));
            let json = serde_json::to_string(&w).unwrap();
            assert_eq!(json, format!("\"{}\"", w.as_str()));
        }
        assert_eq!(Wire::parse("responses"), None);
    }

    #[test]
    fn a_custom_provider_file_parses_with_all_four_wires_at_one_base() {
        let text = r#"
            [providers.canned-test]
            base_url = "http://127.0.0.1:9/v1"
            wires = ["anthropic", "openai-chat", "openai-responses", "gemini"]
        "#;
        let reg = Registry::with_custom(text).unwrap();
        let p = reg.get("canned-test").unwrap();
        assert!(p.custom);
        assert_eq!(p.auth, AuthKind::ApiKey);
        for w in Wire::ALL {
            assert_eq!(p.base_for(w), Some("http://127.0.0.1:9/v1"));
        }
        assert_eq!(reg.custom().count(), 1);
    }

    #[test]
    fn a_custom_file_is_refused_by_name_for_each_malformation() {
        let cases = [
            (
                "[providers.openai]\nbase_url=\"https://x\"\nwires=[\"openai-chat\"]",
                "built in",
            ),
            (
                "[providers.Bad]\nbase_url=\"https://x\"\nwires=[\"openai-chat\"]",
                "not valid",
            ),
            (
                "[providers.x]\nbase_url=\"ftp://x\"\nwires=[\"openai-chat\"]",
                "http",
            ),
            (
                "[providers.x]\nbase_url=\"https://x\"\nwires=[\"soap\"]",
                "unknown wire",
            ),
            (
                "[providers.x]\nbase_url=\"https://x\"\nwires=[]",
                "no wires",
            ),
            (
                "[providers.x]\nbase_url=\"https://x\"\nwires=[\"gemini\"]\nauth=\"oauth\"",
                "auth",
            ),
            (
                "[providers.x]\nbase_url=\"https://x\"\nwires=[\"gemini\"]\nkey=\"sk\"",
                "parse",
            ),
        ];
        for (text, needle) in cases {
            let err = Registry::with_custom(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{text:?} → {err}");
        }
    }

    #[test]
    fn a_credential_id_is_a_provider_and_an_optional_label() {
        let c = CredentialId::parse("openrouter:work").unwrap();
        assert_eq!(
            (c.provider.as_str(), c.label.as_deref()),
            ("openrouter", Some("work"))
        );
        assert_eq!(c.to_string(), "openrouter:work");
        assert_eq!(
            CredentialId::parse("groq").unwrap(),
            CredentialId::default_for("groq")
        );
        for bad in ["", "open router", "a:", ":b", "a:b:c", "A:b", "a:B"] {
            assert!(CredentialId::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_stated_credential_order_parses_and_must_name_its_own_provider() {
        let reg = Registry::with_custom(
            "[credentials]\nopenrouter = [\"openrouter:work\", \"openrouter\"]\n",
        )
        .unwrap();
        let order: Vec<String> = reg
            .credential_order("openrouter")
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(order, ["openrouter:work", "openrouter"]);
        assert!(reg.credential_order("groq").is_none());
        for (text, needle) in [
            (
                "[credentials]\nopenrouter = [\"groq:work\"]\n",
                "not a credential id",
            ),
            ("[credentials]\nnope = [\"nope\"]\n", "neither built in"),
        ] {
            let e = Registry::with_custom(text).unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        }
    }

    /// **How a provider reads its key is row data.** Anthropic's own API takes the key in
    /// `x-api-key` (what its SDKs send for an API key; its docs also accept `Authorization:
    /// Bearer`), while the Anthropic-compatible gateways document `Authorization: Bearer` alone —
    /// so the header is a column of the row, never a branch on the provider's name.
    #[test]
    fn each_provider_states_the_header_its_key_rides_and_a_custom_one_may_choose() {
        let reg = Registry::seed();
        assert_eq!(reg.get("anthropic").unwrap().key_header, KeyHeader::XApiKey);
        for p in PROVIDERS.iter().filter(|p| p.id != "anthropic") {
            assert_eq!(p.key_header, KeyHeader::Bearer, "{}", p.id);
        }
        let reg = Registry::with_custom(
            "[providers.gw]\nbase_url=\"https://gw.example\"\nwires=[\"anthropic\"]\n\
             key_header=\"x-api-key\"\n\n[providers.plain]\nbase_url=\"https://p.example/v1\"\n\
             wires=[\"openai-chat\"]\n",
        )
        .unwrap();
        assert_eq!(reg.get("gw").unwrap().key_header, KeyHeader::XApiKey);
        assert_eq!(reg.get("plain").unwrap().key_header, KeyHeader::Bearer);
        let err = Registry::with_custom(
            "[providers.x]\nbase_url=\"https://x\"\nwires=[\"anthropic\"]\nkey_header=\"cookie\"",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("key_header") && err.contains("x-api-key"),
            "{err}"
        );
        // And it survives `marion login custom`'s rewrite of the file.
        let mut gw = reg.get("gw").unwrap().clone();
        gw.custom = true;
        let text = render_custom(&[gw.clone()], &BTreeMap::new());
        assert_eq!(parse_custom(&text).unwrap(), vec![gw]);
    }

    #[test]
    fn a_rendered_custom_file_parses_back_to_the_same_providers() {
        let a = custom_provider(
            "my-gw",
            "https://gw.example/v1",
            &[Wire::OpenAiChat, Wire::AnthropicMessages],
            AuthKind::ApiKey,
            Some("My \"gateway\""),
            Some("MY_GW_KEY"),
        )
        .unwrap();
        let b = custom_provider(
            "box",
            "http://127.0.0.1:8080/v1",
            &[Wire::OpenAiChat],
            AuthKind::None,
            None,
            None,
        )
        .unwrap();
        let mut order = BTreeMap::new();
        order.insert(
            "openrouter".to_string(),
            vec![
                CredentialId::parse("openrouter:work").unwrap(),
                CredentialId::default_for("openrouter"),
            ],
        );
        let text = render_custom(&[a.clone(), b.clone()], &order);
        assert_eq!(parse_custom(&text).unwrap(), vec![b, a]);
        assert_eq!(
            Registry::with_custom(&text).unwrap().credential_orders(),
            &order
        );
    }
}
