//! **What a harness inherits from the operator's environment, and what it does not.**
//!
//! A managed harness used to inherit the supervisor's whole environment, and with it every
//! credential the operator's shell held: cloud keys, a GitHub token, an npm token, the SSH agent,
//! another vendor's API key. The harness needs none of those to run, and every shell command its
//! model runs would inherit them all.
//!
//! So each row states its [`LoginEnv`]: the variables its own login reads (its API-key and token
//! variables, its auth switches, the cloud credentials a Bedrock or Vertex login needs) and whether
//! it reaches many providers on the operator's own login. A variable that looks like a credential
//! ([`is_credential`]) is withheld unless it is the row's own; the operator's own login is inherited
//! exactly as the harness would read it (marion never hides a login source). Under a canned or
//! endpoint launch marion supplies the whole route, so every credential and every login switch the
//! operator's environment holds is withheld. Everything else — `HOME`, `PATH`, `TERM`, locale,
//! proxies, config directories — passes untouched. The operator widens it per agent type with
//! `env_passthrough`.

use std::ffi::{OsStr, OsString};

use crate::auth::Auth;

/// A variable a row's login reads: an exact name, or a prefix ending in `*`, kept only while the
/// variable named by `when_set` is set in the same environment (a Bedrock login's `AWS_*` is kept
/// only where `CLAUDE_CODE_USE_BEDROCK` says the operator's login is Bedrock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvGrant {
    pub pattern: &'static str,
    pub when_set: Option<&'static str>,
}

impl EnvGrant {
    /// Always kept on the operator's own login.
    pub const fn always(pattern: &'static str) -> Self {
        Self {
            pattern,
            when_set: None,
        }
    }

    /// Kept on the operator's own login while `switch` is set.
    pub const fn when(pattern: &'static str, switch: &'static str) -> Self {
        Self {
            pattern,
            when_set: Some(switch),
        }
    }
}

/// [`crate::spec::HarnessSpec::login_env`]: what the harness's own login reads from the
/// environment. Stated on every row (`every_row_states_its_login_env`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginEnv {
    /// The harness's own login variables: its keys and tokens, its auth switches, its endpoint.
    pub login: &'static [EnvGrant],
    /// Whether the harness reaches many providers on the operator's own login (opencode, goose,
    /// cline, pi, an ACP agent), and so keeps every provider API key under [`Auth::Inherited`].
    pub any_provider: bool,
}

/// Credential-shaped names every launch withholds unless its row's login or the operator's
/// passthrough keeps them: exact names and `*`-prefixes, then suffixes. A harness's own login
/// variables need no entry here: every row's [`LoginEnv`] is another row's foreign credential
/// ([`is_foreign_login`]).
const CREDENTIALS: &[&str] = &[
    "AWS_*",
    "AZURE_*",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "TWINE_PASSWORD",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    "KUBECONFIG",
    "SSH_AUTH_SOCK",
    "DOCKER_AUTH_CONFIG",
    "VAULT_TOKEN",
];
const CREDENTIAL_SUFFIXES: &[&str] = &[
    "_API_KEY",
    "_API_TOKEN",
    "_ACCESS_TOKEN",
    "_AUTH_TOKEN",
    "_SECRET_KEY",
    "_SECRET",
    "_PASSWORD",
];

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Whether `name` looks like a credential: on the fixed list, or ending like a key or a token.
pub fn is_credential(name: &str) -> bool {
    CREDENTIALS.iter().any(|p| matches(p, name))
        || CREDENTIAL_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Whether `name` is a variable some harness row's login reads — another vendor's key, token or
/// login switch, to any harness but that one.
pub fn is_foreign_login(name: &str) -> bool {
    marion_core::harness::Harness::ALL.iter().any(|h| {
        crate::adapter::harness_spec(*h)
            .login_env
            .login
            .iter()
            .any(|g| matches(g.pattern, name))
    })
}

/// Whether `name` is a provider's API key: a seed provider's import variable, or any `*_API_KEY`.
pub fn is_provider_key(name: &str) -> bool {
    name.ends_with("_API_KEY")
        || marion_core::provider::PROVIDERS
            .iter()
            .any(|p| p.import_env == Some(name))
}

/// Whether `name`'s value is itself a secret — a key, a token, a password — rather than a path, a
/// switch or an endpoint that merely belongs to a login (`SSH_AUTH_SOCK`, `AWS_REGION`,
/// `ANTHROPIC_BASE_URL`), which a recording keeps.
pub fn holds_secret(name: &str) -> bool {
    ["_KEY", "_TOKEN", "_SECRET", "_PASSWORD"]
        .iter()
        .any(|s| name.ends_with(s))
}

/// The values among `env` a recording must never keep: every variable's that holds a secret
/// ([`holds_secret`]: its own key, the operator's login token, the node token), deduplicated. A
/// value too short to be a key (under 8 bytes) is left out, as the redaction rule leaves it alone.
pub fn credential_values(env: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for (name, value) in env {
        let secret = holds_secret(&name);
        if secret && value.len() >= 8 && !values.contains(&value) {
            values.push(value);
        }
    }
    values
}

/// **The filter one launch applies to what it inherits**: the row's login, the launch's auth mode
/// and the operator's passthrough for its agent type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritFilter {
    pub login: LoginEnv,
    pub auth: Auth,
    pub passthrough: Vec<String>,
}

impl InheritFilter {
    /// Whether this launch withholds `name`, with `is_set` answering whether another variable is
    /// set in the same environment (for [`EnvGrant::when_set`]).
    pub fn withholds(&self, name: &str, is_set: &dyn Fn(&str) -> bool) -> bool {
        if self.passthrough.iter().any(|p| matches(p, name)) {
            return false;
        }
        let own = self
            .login
            .login
            .iter()
            .any(|g| matches(g.pattern, name) && g.when_set.is_none_or(is_set));
        match self.auth {
            // marion supplies the whole route: no credential and no login switch of the
            // operator's may redirect it.
            Auth::Canned | Auth::Endpoint => own || is_credential(name) || is_foreign_login(name),
            Auth::Inherited if own => false,
            // A harness that reaches many providers keeps their keys and their login variables,
            // and still no unrelated credential.
            Auth::Inherited if self.login.any_provider => {
                is_credential(name) && !is_provider_key(name)
            }
            Auth::Inherited => is_credential(name) || is_foreign_login(name),
        }
    }

    /// The names among `inherited` this launch removes, `set` excepted: what marion sets itself is
    /// never withheld from it.
    pub fn removals(
        &self,
        inherited: &[(OsString, OsString)],
        set: &[(String, String)],
    ) -> Vec<OsString> {
        let is_set = |k: &str| inherited.iter().any(|(n, _)| n == k);
        inherited
            .iter()
            .filter_map(|(k, _)| {
                let name = k.to_str()?;
                (!set.iter().any(|(n, _)| n == name) && self.withholds(name, &is_set))
                    .then(|| k.clone())
            })
            .collect()
    }

    /// `env` with every variable this launch withholds removed — for a launch that builds its
    /// environment whole (the native lane) rather than inheriting one.
    pub fn filter(&self, env: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
        let gone = self.removals(&env, &[]);
        env.into_iter()
            .filter(|(k, _)| !gone.iter().any(|g| g.as_os_str() == OsStr::new(k)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: LoginEnv = LoginEnv {
        login: &[
            EnvGrant::always("ANTHROPIC_API_KEY"),
            EnvGrant::always("CLAUDE_CODE_USE_BEDROCK"),
            EnvGrant::when("AWS_*", "CLAUDE_CODE_USE_BEDROCK"),
        ],
        any_provider: false,
    };

    fn env(pairs: &[&str]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|k| (OsString::from(k), OsString::from("v")))
            .collect()
    }

    fn kept(f: &InheritFilter, inherited: &[&str]) -> Vec<String> {
        f.filter(env(inherited))
            .into_iter()
            .map(|(k, _)| k.into_string().unwrap())
            .collect()
    }

    const OPERATOR: &[&str] = &[
        "HOME",
        "PATH",
        "TERM",
        "LANG",
        "HTTPS_PROXY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GH_TOKEN",
        "NPM_TOKEN",
        "SSH_AUTH_SOCK",
        "KUBECONFIG",
        "STRIPE_SECRET_KEY",
    ];

    /// **On the operator's own login a single-provider harness keeps its own key and nothing
    /// else credential-shaped**; the runtime (`HOME`, `PATH`, locale, proxies) always passes.
    #[test]
    fn a_single_provider_harness_keeps_its_own_login_and_no_other_credential() {
        let f = InheritFilter {
            login: CLAUDE,
            auth: Auth::Inherited,
            passthrough: vec![],
        };
        assert_eq!(
            kept(&f, OPERATOR),
            [
                "HOME",
                "PATH",
                "TERM",
                "LANG",
                "HTTPS_PROXY",
                "ANTHROPIC_API_KEY"
            ]
        );
    }

    /// A Bedrock login's `AWS_*` is the operator's own login where their switch says so, and an
    /// operator credential where it does not.
    #[test]
    fn a_conditional_grant_is_kept_only_while_its_switch_is_set() {
        let f = InheritFilter {
            login: CLAUDE,
            auth: Auth::Inherited,
            passthrough: vec![],
        };
        assert!(kept(&f, &["AWS_SECRET_ACCESS_KEY"]).is_empty());
        assert_eq!(
            kept(&f, &["CLAUDE_CODE_USE_BEDROCK", "AWS_SECRET_ACCESS_KEY"]),
            ["CLAUDE_CODE_USE_BEDROCK", "AWS_SECRET_ACCESS_KEY"]
        );
    }

    /// A multi-provider harness keeps every provider key but still no unrelated credential.
    #[test]
    fn a_multi_provider_harness_keeps_provider_keys_and_drops_the_rest() {
        let f = InheritFilter {
            login: LoginEnv {
                login: &[],
                any_provider: true,
            },
            auth: Auth::Inherited,
            passthrough: vec![],
        };
        assert_eq!(
            kept(&f, OPERATOR),
            [
                "HOME",
                "PATH",
                "TERM",
                "LANG",
                "HTTPS_PROXY",
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY"
            ]
        );
    }

    /// **A canned or endpoint launch strips all of it**: marion supplies the route, so neither the
    /// harness's own key nor its login switch nor any other credential is inherited.
    #[test]
    fn a_canned_or_endpoint_launch_inherits_no_credential_and_no_login_switch() {
        for auth in [Auth::Canned, Auth::Endpoint] {
            let f = InheritFilter {
                login: CLAUDE,
                auth,
                passthrough: vec![],
            };
            let mut operator = OPERATOR.to_vec();
            operator.push("CLAUDE_CODE_USE_BEDROCK");
            assert_eq!(
                kept(&f, &operator),
                ["HOME", "PATH", "TERM", "LANG", "HTTPS_PROXY"],
                "{auth:?}"
            );
        }
    }

    /// **Every row's login is every other row's foreign credential**: a codex node on the
    /// operator's login gets neither claude's OAuth token nor its endpoint switch, which claude's
    /// own row keeps; a multi-provider harness keeps both.
    #[test]
    fn one_rows_login_is_withheld_from_every_other_single_provider_row() {
        let on_login = |h| InheritFilter {
            login: crate::adapter::harness_spec(h).login_env,
            auth: Auth::Inherited,
            passthrough: vec![],
        };
        use marion_core::harness::Harness;
        let theirs = ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_BASE_URL"];
        assert!(kept(&on_login(Harness::Codex), &theirs).is_empty());
        assert_eq!(kept(&on_login(Harness::ClaudeCode), &theirs), theirs);
        assert_eq!(
            kept(&on_login(Harness::OpenCode), &["ANTHROPIC_BASE_URL"]),
            ["ANTHROPIC_BASE_URL"]
        );
    }

    /// A recording scrubs every credential-shaped value and the node token's, once each, and
    /// never a runtime value or a value too short to be a key.
    #[test]
    fn credential_values_are_the_secrets_a_recording_must_scrub() {
        let env = [
            ("PATH", "/usr/bin:/bin:/opt/homebrew/bin"),
            ("ANTHROPIC_API_KEY", "sk-ant-operator-000"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-operator-111"),
            ("MARION_NODE_TOKEN", "node-token-222"),
            ("GH_TOKEN", "sk-ant-operator-000"),
            ("NPM_TOKEN", "short"),
            ("SSH_AUTH_SOCK", "/var/run/launchd/Listeners"),
            ("ANTHROPIC_BASE_URL", "http://127.0.0.1:8099"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(
            credential_values(env),
            [
                "sk-ant-operator-000",
                "oauth-operator-111",
                "node-token-222"
            ]
        );
    }

    /// The operator's passthrough keeps what they named, and marion never removes what it set.
    #[test]
    fn passthrough_and_marions_own_variables_are_never_withheld() {
        let f = InheritFilter {
            login: CLAUDE,
            auth: Auth::Canned,
            passthrough: vec!["GH_TOKEN".into(), "CORP_*".into()],
        };
        assert_eq!(
            kept(&f, &["GH_TOKEN", "CORP_API_KEY", "NPM_TOKEN"]),
            ["GH_TOKEN", "CORP_API_KEY"]
        );
        let set = vec![("ANTHROPIC_API_KEY".to_string(), String::new())];
        assert!(f.removals(&env(&["ANTHROPIC_API_KEY"]), &set).is_empty());
    }
}
