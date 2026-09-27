//! **Endpoint resolution**: which provider, wire, base URL, key and model a node runs on when its
//! agent type or its model names a provider — the one decision behind endpoint mode, made in one
//! place for a child (`run::run_spawn_watched`) and a root (`root::prepare`) alike.
//!
//! A provider is named by the agent type's `provider` field, or by a `<provider>:<model>` prefix on
//! the model — split on the first colon only, and only when the prefix is a registry id, so
//! `ollama:qwen3:32b` is ollama's `qwen3:32b` and a bare `qwen3:32b` stays a model. A request's own
//! prefix wins over the type's field: the spawn is the more specific statement.
//!
//! Every refusal names what to do: an unknown provider names `marion login --list`, a missing key
//! names `marion login <id>`, and a provider serving no wire the harness can be pointed at lists
//! both sides. Translating between wires (a gateway) is a later phase; today a route is `native`.

use marion_core::agent_type::AgentType;
use marion_core::harness::Harness;
use marion_core::provider::{CredentialId, Registry, Wire};
use marion_harness::{Auth, LaunchSpec};

use crate::credentials::{CredentialStore, Secret};

/// The only route this phase resolves: the provider serves the harness's wire itself.
pub const ROUTE_NATIVE: &str = "native";

/// A resolved endpoint. `Debug` shows the key as `***` ([`Secret`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub provider: String,
    pub wire: Wire,
    pub base_url: String,
    /// `None` for a provider that authenticates nothing (a local server).
    pub key: Option<Secret>,
    /// Which of the provider's credentials [`Self::key`] is — recorded, never the key itself.
    pub credential: CredentialId,
    /// The credentials after it in the order tried — what a rotation would move to next. Nothing
    /// moves through them yet.
    pub fallbacks: Vec<CredentialId>,
    /// The model id the provider is asked for, prefix removed.
    pub model: String,
}

/// Why a node naming a provider cannot run on it. No variant carries a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error(
        "provider `{0}` is neither built in nor in your providers.toml; `marion login --list` \
         shows the ones marion knows"
    )]
    UnknownProvider(String),
    #[error(
        "provider `{0}` needs a model to ask for; name one (`-m {0}:<model>`, or `model` on the \
         agent type)"
    )]
    NoModel(String),
    #[error(
        "no key is stored for provider `{provider}` (tried {tried}); run `marion login {provider}`"
    )]
    LoggedOut { provider: String, tried: String },
    #[error(
        "{harness} speaks {harness_wires} and provider `{provider}` serves {provider_wires}; no \
         wire is shared, and translating between them is not built yet"
    )]
    NoSharedWire {
        harness: Harness,
        provider: String,
        harness_wires: String,
        provider_wires: String,
    },
    #[error("cannot read provider configuration: {0}")]
    Config(String),
}

fn wire_list(wires: &[Wire]) -> String {
    if wires.is_empty() {
        return "no endpoint wire".to_string();
    }
    wires
        .iter()
        .map(|w| w.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The provider a launch names, and the model with any prefix removed — or `None` where it names
/// no provider and runs canned or live as before.
fn named_provider<'m>(
    registry: &Registry,
    model_req: Option<&'m str>,
    agent_type: &'m AgentType,
) -> Option<(String, Option<String>)> {
    if let Some((p, m)) = model_req.and_then(|m| registry.split_model(m)) {
        return Some((p.id.clone(), Some(m.to_string())));
    }
    let type_model = agent_type.model.as_deref();
    if let Some(p) = &agent_type.provider {
        let model = model_req
            .or(type_model)
            .map(|m| match registry.split_model(m) {
                Some((q, rest)) if q.id == *p => rest.to_string(),
                _ => m.to_string(),
            });
        return Some((p.clone(), model));
    }
    if model_req.is_none()
        && let Some((p, m)) = type_model.and_then(|m| registry.split_model(m))
    {
        return Some((p.id.clone(), Some(m.to_string())));
    }
    None
}

/// The order `provider`'s credentials are tried in: the type's own list, else the user's stated
/// `[credentials]` order, else login order with the unlabelled id last — never empty.
fn credential_order(
    provider: &str,
    agent_type: &AgentType,
    registry: &Registry,
    logins: &[CredentialId],
) -> Vec<CredentialId> {
    let typed: Vec<CredentialId> = agent_type
        .credentials
        .iter()
        .filter_map(|c| CredentialId::parse(c))
        .filter(|c| c.provider == provider)
        .collect();
    if !typed.is_empty() {
        return typed;
    }
    if let Some(order) = registry
        .credential_order(provider)
        .filter(|o| !o.is_empty())
    {
        return order.to_vec();
    }
    let mut order: Vec<CredentialId> = logins
        .iter()
        .filter(|c| c.provider == provider)
        .cloned()
        .collect();
    let default = CredentialId::default_for(provider);
    if !order.contains(&default) {
        order.push(default);
    }
    order
}

/// Resolve a launch's endpoint against the seed-plus-user registry and a store, with no login
/// index: the provider's unlabelled credential alone, unless the type or the registry orders more.
pub fn resolve_endpoint(
    model_req: Option<&str>,
    agent_type: &AgentType,
    harness_wires: &[Wire],
    registry: &Registry,
    store: &dyn CredentialStore,
) -> Result<Option<Endpoint>, EndpointError> {
    resolve_endpoint_with(model_req, agent_type, harness_wires, registry, &[], store)
}

/// Resolve a launch's endpoint. `Ok(None)` is a launch that names no provider. `logins` is the
/// user's login index, the last word on credential order.
pub fn resolve_endpoint_with(
    model_req: Option<&str>,
    agent_type: &AgentType,
    harness_wires: &[Wire],
    registry: &Registry,
    logins: &[CredentialId],
    store: &dyn CredentialStore,
) -> Result<Option<Endpoint>, EndpointError> {
    let Some((id, model)) = named_provider(registry, model_req, agent_type) else {
        return Ok(None);
    };
    let provider = registry
        .get(&id)
        .ok_or_else(|| EndpointError::UnknownProvider(id.clone()))?;
    let model = model
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| EndpointError::NoModel(id.clone()))?;
    let (wire, base_url) = harness_wires
        .iter()
        .find_map(|w| provider.base_for(*w).map(|b| (*w, b.to_string())))
        .ok_or_else(|| EndpointError::NoSharedWire {
            harness: agent_type.harness,
            provider: id.clone(),
            harness_wires: wire_list(harness_wires),
            provider_wires: provider.wire_list(),
        })?;
    let order = credential_order(&id, agent_type, registry, logins);
    let (credential, key, fallbacks) = if provider.auth.needs_credential() {
        let mut found = None;
        for (i, c) in order.iter().enumerate() {
            let key = store
                .get(&c.to_string())
                .map_err(|e| EndpointError::Config(e.to_string()))?;
            if let Some(k) = key {
                found = Some((c.clone(), Some(k), order[i + 1..].to_vec()));
                break;
            }
        }
        found.ok_or_else(|| EndpointError::LoggedOut {
            provider: id.clone(),
            tried: order
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        })?
    } else {
        (CredentialId::default_for(&id), None, Vec::new())
    };
    Ok(Some(Endpoint {
        provider: id,
        wire,
        base_url,
        key,
        credential,
        fallbacks,
        model,
    }))
}

/// [`resolve_endpoint`] against the user's own registry and credential store — what a launch
/// calls. The store and registry are opened only when the launch names a provider.
pub fn resolve_for_launch(
    model_req: Option<&str>,
    agent_type: &AgentType,
    harness_wires: &[Wire],
) -> Result<Option<Endpoint>, EndpointError> {
    let registry = crate::credentials::user_registry().map_err(EndpointError::Config)?;
    if named_provider(&registry, model_req, agent_type).is_none() {
        return Ok(None);
    }
    let store =
        crate::credentials::default_store().map_err(|e| EndpointError::Config(e.to_string()))?;
    let logins = crate::credentials::Logins::user()
        .and_then(|l| l.all())
        .map_err(|e| EndpointError::Config(e.to_string()))?;
    resolve_endpoint_with(
        model_req,
        agent_type,
        harness_wires,
        &registry,
        &logins,
        store.as_ref(),
    )
}

/// Point a launch at `ep`: endpoint auth, the provider's base and key, the bare model — with the
/// supervisor's own mode and endpoint kept for the bridge the node starts.
pub fn apply(launch: &mut LaunchSpec, ep: &Endpoint) {
    launch.extra.tree_auth = Some(launch.auth);
    launch.extra.tree_base_url = launch.base_url.take();
    launch.auth = Auth::Endpoint;
    launch.base_url = Some(ep.base_url.clone());
    // A key-less local server still gets the placeholder canned mode uses: several harnesses
    // refuse to start over an empty credential slot, and the server authenticates nothing.
    launch.api_key = Some(match &ep.key {
        Some(k) => k.expose().to_string(),
        None => crate::run::PLACEHOLDER_API_KEY.to_string(),
    });
    launch.model = Some(ep.model.clone());
    launch.wire = Some(ep.wire);
    launch.provider = Some(ep.provider.clone());
}

/// The model a resumed node asks for: its journaled provider back in front of the provider's model
/// id, so the relaunch re-resolves that provider — and re-reads its key — rather than running canned
/// or live under a provider's model id. A node with no provider asks for its model as recorded.
pub fn resume_model(
    model: Option<&str>,
    provider: Option<&str>,
    agent_type: &AgentType,
) -> Option<String> {
    let adapter =
        marion_harness::adapter_for_type(agent_type.harness, agent_type.acp_agent.as_deref());
    match (model, provider, adapter) {
        (Some(m), Some(p), Ok(a)) => Some(format!("{p}:{}", a.endpoint_model(m))),
        (Some(m), Some(p), Err(_)) => Some(format!("{p}:{m}")),
        (m, _, _) => m.map(str::to_string),
    }
}

/// `text` with every occurrence of `key` replaced by `***` — applied to what an endpoint node
/// wrote before it is kept anywhere. Keys shorter than eight characters are not searched for:
/// replacing a short string would mangle ordinary output and such a key protects nothing.
pub fn redact(text: &str, key: &str) -> String {
    if key.len() < 8 || !text.contains(key) {
        return text.to_string();
    }
    text.replace(key, "***")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{CredentialError, Secret};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<String, String>>);

    impl MemStore {
        fn with(id: &str, key: &str) -> Self {
            let s = MemStore::default();
            s.0.lock().unwrap().insert(id.into(), key.into());
            s
        }
    }

    impl CredentialStore for MemStore {
        fn get(&self, p: &str) -> Result<Option<Secret>, CredentialError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(p)
                .map(|k| Secret::new(k).unwrap()))
        }
        fn put(&self, p: &str, k: &Secret) -> Result<(), CredentialError> {
            self.0.lock().unwrap().insert(p.into(), k.expose().into());
            Ok(())
        }
        fn delete(&self, p: &str) -> Result<bool, CredentialError> {
            Ok(self.0.lock().unwrap().remove(p).is_some())
        }
        fn describe(&self) -> String {
            "memory".into()
        }
    }

    fn ty(harness: Harness, provider: Option<&str>, model: Option<&str>) -> AgentType {
        let mut t = marion_core::agent_type::builtin("codex-impl").unwrap();
        t.harness = harness;
        t.provider = provider.map(str::to_string);
        t.model = model.map(str::to_string);
        t
    }

    const CHAT: &[Wire] = &[Wire::OpenAiChat];
    const ANTHROPIC: &[Wire] = &[Wire::AnthropicMessages];

    #[test]
    fn a_launch_naming_no_provider_resolves_to_none_and_reads_no_key() {
        let reg = Registry::seed();
        let store = MemStore::default();
        for model in [None, Some("gpt-5"), Some("qwen3:32b")] {
            let got = resolve_endpoint(model, &ty(Harness::Codex, None, None), CHAT, &reg, &store);
            assert_eq!(got, Ok(None), "{model:?}");
        }
    }

    #[test]
    fn a_model_prefix_names_the_provider_and_is_stripped_on_the_first_colon_only() {
        let reg = Registry::seed();
        let ep = resolve_endpoint(
            Some("ollama:qwen3:32b"),
            &ty(Harness::OpenCode, None, None),
            CHAT,
            &reg,
            &MemStore::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ep.provider, "ollama");
        assert_eq!(ep.model, "qwen3:32b");
        assert_eq!(ep.wire, Wire::OpenAiChat);
        assert_eq!(ep.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(ep.key, None, "a local server needs no key");
    }

    #[test]
    fn the_agent_types_provider_names_it_and_the_first_shared_wire_in_harness_order_wins() {
        let reg = Registry::seed();
        let store = MemStore::with("openrouter", "sk-or-test-key");
        let ep = resolve_endpoint(
            Some("anthropic/claude-sonnet-4"),
            &ty(Harness::ClaudeCode, Some("openrouter"), None),
            ANTHROPIC,
            &reg,
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ep.wire, Wire::AnthropicMessages);
        assert_eq!(ep.base_url, "https://openrouter.ai/api");
        assert_eq!(ep.model, "anthropic/claude-sonnet-4");
        assert_eq!(ep.key.unwrap().expose(), "sk-or-test-key");
        // The type's own default model, with a redundant prefix of the same provider, is fine too.
        let ep = resolve_endpoint(
            None,
            &ty(
                Harness::OpenCode,
                Some("openrouter"),
                Some("openrouter:qwen/qwen3-coder"),
            ),
            CHAT,
            &reg,
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ep.model, "qwen/qwen3-coder");
    }

    #[test]
    fn a_requests_own_prefix_wins_over_the_types_provider() {
        let reg = Registry::seed();
        let store = MemStore::with("groq", "gsk-test-key-1");
        let ep = resolve_endpoint(
            Some("groq:llama-3.3-70b"),
            &ty(Harness::OpenCode, Some("openrouter"), None),
            CHAT,
            &reg,
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ep.provider, "groq");
    }

    #[test]
    fn each_refusal_names_what_to_run() {
        let reg = Registry::seed();
        let empty = MemStore::default();
        let err = |r: Result<Option<Endpoint>, EndpointError>| r.unwrap_err().to_string();
        let e = err(resolve_endpoint(
            Some("m"),
            &ty(Harness::Codex, Some("nope"), None),
            CHAT,
            &reg,
            &empty,
        ));
        assert!(
            e.contains("`nope`") && e.contains("marion login --list"),
            "{e}"
        );
        let e = err(resolve_endpoint(
            Some("openai:gpt-5"),
            &ty(Harness::Codex, None, None),
            &[Wire::OpenAiResponses],
            &reg,
            &empty,
        ));
        assert!(e.contains("marion login openai"), "{e}");
        let e = err(resolve_endpoint(
            None,
            &ty(Harness::Codex, Some("openai"), None),
            &[Wire::OpenAiResponses],
            &reg,
            &empty,
        ));
        assert!(e.contains("needs a model"), "{e}");
        // Codex speaks Responses alone, and groq serves Chat alone: both sides are named, and the
        // wire check comes before the key check, so nobody logs in to learn it cannot work.
        let e = err(resolve_endpoint(
            Some("groq:llama"),
            &ty(Harness::Codex, None, None),
            &[Wire::OpenAiResponses],
            &reg,
            &empty,
        ));
        assert!(
            e.contains("codex") && e.contains("openai-responses") && e.contains("openai-chat"),
            "{e}"
        );
        let e = err(resolve_endpoint(
            Some("groq:llama"),
            &ty(Harness::Acp, None, None),
            &[],
            &reg,
            &empty,
        ));
        assert!(e.contains("no endpoint wire"), "{e}");
    }

    #[test]
    fn apply_points_the_launch_at_the_provider_and_keeps_the_tree_mode_for_the_bridge() {
        let mut launch = LaunchSpec {
            cwd: "/wt".into(),
            model: Some("groq:llama".into()),
            prompt: String::new(),
            tools: vec![],
            allowed_tools: vec![],
            mcp: marion_harness::McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:9/v1".into()),
            api_key: Some("dummy".into()),
            auth: Auth::Canned,
            config_dir: "/c".into(),
            resume: None,
            wire: None,
            provider: None,
            extra: Default::default(),
        };
        let ep = Endpoint {
            provider: "groq".into(),
            wire: Wire::OpenAiChat,
            base_url: "https://api.groq.com/openai/v1".into(),
            key: Some(Secret::new("gsk-test-key-1").unwrap()),
            credential: CredentialId::default_for("groq"),
            fallbacks: vec![],
            model: "llama".into(),
        };
        apply(&mut launch, &ep);
        assert_eq!(launch.auth, Auth::Endpoint);
        assert_eq!(
            launch.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
        assert_eq!(launch.api_key.as_deref(), Some("gsk-test-key-1"));
        assert_eq!(launch.model.as_deref(), Some("llama"));
        assert_eq!(launch.wire, Some(Wire::OpenAiChat));
        assert_eq!(launch.provider.as_deref(), Some("groq"));
        assert_eq!(launch.extra.tree_auth, Some(Auth::Canned));
        assert_eq!(
            launch.extra.tree_base_url.as_deref(),
            Some("http://127.0.0.1:9/v1")
        );
    }

    /// **A resumed endpoint node re-resolves its provider**: the model it re-requests carries the
    /// journaled provider as its prefix, with the adapter's own spelling undone.
    #[test]
    fn a_resumed_endpoint_node_asks_for_its_provider_and_model_again() {
        let opencode = ty(Harness::OpenCode, None, None);
        assert_eq!(
            resume_model(
                Some("marion/anthropic/claude-sonnet-4"),
                Some("openrouter"),
                &opencode
            )
            .as_deref(),
            Some("openrouter:anthropic/claude-sonnet-4")
        );
        let codex = ty(Harness::Codex, None, None);
        assert_eq!(
            resume_model(Some("gpt-5.1-codex"), Some("openai"), &codex).as_deref(),
            Some("openai:gpt-5.1-codex")
        );
        assert_eq!(
            resume_model(Some("marion/canned-1"), None, &opencode).as_deref(),
            Some("marion/canned-1"),
            "a canned node resumes exactly as before"
        );
        // And the prefix round-trips through the resolver to the same provider and model.
        let reg = Registry::seed();
        let store = MemStore::with("openrouter", "sk-or-test-key");
        let ep = resolve_endpoint(
            Some("openrouter:anthropic/claude-sonnet-4"),
            &opencode,
            CHAT,
            &reg,
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (ep.provider.as_str(), ep.model.as_str()),
            ("openrouter", "anthropic/claude-sonnet-4")
        );
    }

    /// **Which of a provider's credentials a launch uses**: the type's own `credentials` list,
    /// else the `[credentials]` order the user stated, else login order — the first with a stored
    /// key, with the rest kept as the fallbacks a later rotation would move through.
    #[test]
    fn the_first_credential_with_a_key_in_the_stated_order_is_used() {
        let reg = Registry::with_custom(
            "[credentials]\nopenrouter = [\"openrouter:work\", \"openrouter:personal\"]\n",
        )
        .unwrap();
        let id = |s: &str| CredentialId::parse(s).unwrap();
        let store = MemStore::with("openrouter:personal", "sk-personal-1");
        let logins = vec![id("openrouter:personal"), id("openrouter:work")];
        let ep = resolve_endpoint_with(
            Some("openrouter:m"),
            &ty(Harness::OpenCode, None, None),
            CHAT,
            &reg,
            &logins,
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            ep.credential,
            id("openrouter:personal"),
            "work has no key, so personal"
        );
        assert_eq!(ep.key.unwrap().expose(), "sk-personal-1");
        assert!(ep.fallbacks.is_empty());
        // The type's own list wins over the file's order.
        store
            .put("openrouter:work", &Secret::new("sk-work-1").unwrap())
            .unwrap();
        let mut t = ty(Harness::OpenCode, None, None);
        t.credentials = vec!["openrouter:personal".into(), "openrouter:work".into()];
        let ep = resolve_endpoint_with(Some("openrouter:m"), &t, CHAT, &reg, &logins, &store)
            .unwrap()
            .unwrap();
        assert_eq!(ep.credential, id("openrouter:personal"));
        assert_eq!(ep.fallbacks, vec![id("openrouter:work")]);
        // With no stated order, login order; and a refusal names every id it tried.
        let seed = Registry::seed();
        let ep = resolve_endpoint_with(
            Some("openrouter:m"),
            &ty(Harness::OpenCode, None, None),
            CHAT,
            &seed,
            &logins,
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ep.credential, id("openrouter:personal"));
        let err = resolve_endpoint_with(
            Some("openrouter:m"),
            &ty(Harness::OpenCode, None, None),
            CHAT,
            &reg,
            &logins,
            &MemStore::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("openrouter:work, openrouter:personal") && err.contains("marion login"),
            "{err}"
        );
    }

    #[test]
    fn redaction_removes_the_key_and_leaves_short_keys_alone() {
        let key = "sk-endpoint-test";
        assert_eq!(
            redact(&format!("401: bad key {key} (Bearer {key})"), key),
            "401: bad key *** (Bearer ***)"
        );
        assert_eq!(redact("abc", "abc"), "abc");
        assert_eq!(redact("nothing here", key), "nothing here");
    }
}
