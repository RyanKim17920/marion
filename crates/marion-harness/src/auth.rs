/// Where the node's provider credential comes from, and therefore which endpoint it talks to.
///
/// An enum rather than a bool because an adapter reads *intent*, not a flag: the two modes differ in
/// what marion is entitled to overlay, and a `bool` at the call site would say `true` without saying
/// true of what. §6.4's central MUST is unchanged under either — marion never mutates the user's
/// real harness config — so what varies is only what marion *adds*, never what it edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Auth {
    /// marion mints the credential and points the node at its own canned endpoint. Every M1 path
    /// takes this, and it is the default so that adding the axis changed no existing behaviour.
    #[default]
    Canned,
    /// The node authenticates with the login the operator already has, inherited from marion's own
    /// environment.
    ///
    /// Nothing is *seeded*: no `crates/` code calls `env_clear`, so a child inherits marion's
    /// environment wholesale and marion only ever layers on top. Live mode is therefore the
    /// **absence** of three overlays rather than the presence of a credential store — marion pushes
    /// no key, overrides no base URL, and leaves the harness's own resolution alone.
    Inherited,
    /// The node talks to a real provider endpoint with a key the **user** stored through `marion
    /// login` — the same overlays as [`Auth::Canned`] (isolation, base URL, credential), pointed
    /// somewhere real.
    ///
    /// **Per node, never supervisor-wide.** A launch resolves to it when its agent type or model
    /// names a provider; the supervisor itself still runs canned or inherited, and the bridge a
    /// node starts is told that mode, never this one ([`Auth::from_wire`] refuses `"endpoint"`).
    Endpoint,
}

impl Auth {
    /// The spelling that rides a bridge declaration's `env` block (`MARION_AUTH`).
    ///
    /// A string rather than a bool for the same reason the type is an enum: an MCP `env` block is
    /// `Record<string,string>` on every harness that has one, and `"true"` would say *true of what*.
    pub fn as_wire(self) -> &'static str {
        match self {
            Auth::Canned => "canned",
            Auth::Inherited => "inherited",
            // Spelled so a stray declaration is legible, and refused by `from_wire`: no bridge or
            // supervisor runs in endpoint mode.
            Auth::Endpoint => "endpoint",
        }
    }

    /// Whether marion **overlays** this node's provider: its own config dir as the harness's
    /// isolation, and a base URL and credential of marion's choosing. True of canned and endpoint
    /// alike — they differ in where the overlay points, never in what it replaces — and false of
    /// live mode, which is the removal of every overlay.
    pub const fn overlays(self) -> bool {
        matches!(self, Auth::Canned | Auth::Endpoint)
    }

    /// The inverse, for the bridge reading the declaration marion wrote.
    ///
    /// **An unrecognised value is `None`, and the caller must not treat that as `Inherited`.** The
    /// two failure directions are not symmetric: guessing canned costs a run against an endpoint
    /// that is not there, while guessing live points the operator's real credential somewhere marion
    /// did not choose. Absence — a declaration written before this key existed — is the caller's to
    /// resolve, and every such declaration meant [`Auth::Canned`].
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim() {
            "canned" => Some(Auth::Canned),
            "inherited" => Some(Auth::Inherited),
            // Endpoint is per node; no process is ever told to run the whole tree in it.
            _ => None,
        }
    }
}

/// Phrases a harness prints when it could not authenticate, lowercased.
///
/// **One shared list, not a per-row field, and that is a choice.** The phrases are the vendors'
/// and SDKs' generic auth vocabulary — Google's `Error authenticating`, an HTTP `401`, `Invalid API
/// key` — and the same one recurs across harnesses that share an SDK (gemini and qwen, every
/// OpenAI-compatible client). A field on each row would duplicate them into a dozen rows and still
/// cover nothing for an ACP agent or a user-defined type, which have no row. A false positive costs
/// little: the line is only moved to the front of a refusal that quotes stderr anyway.
const AUTH_FAILURE_MARKERS: &[&str] = &[
    // gemini 0.53.0 on a personal login Google no longer serves (measured, exit 55).
    "error authenticating",
    "ineligibletiererror",
    // gemini with no route at all: "Please set an Auth method in your …/settings.json or specify
    // one of the following environment variables" (measured).
    "please set an auth method",
    "api key not valid",
    "invalid api key",
    "not logged in",
    "please run /login",
    "authentication failed",
    "authentication error",
    "authentication required",
    "unauthorized",
];

/// The first stderr line that says the harness could not authenticate, trimmed and capped.
///
/// Scans the **whole** of `stderr`, not a preview: the line that matters is often behind a banner
/// or ahead of a stack trace, and a fixed-length prefix is where it got lost.
pub fn auth_failure_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| is_auth_failure_line(l))
        .map(|l| l.chars().take(512).collect())
}

fn is_auth_failure_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    AUTH_FAILURE_MARKERS.iter().any(|m| lower.contains(m)) || has_status_401(&lower)
}

/// `401` as a token of its own — not the tail of a port, a pid or an id such as `14010`.
fn has_status_401(lower: &str) -> bool {
    lower.match_indices("401").any(|(i, _)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + 3..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_overlays_like_canned_and_is_never_a_wire_mode() {
        assert!(Auth::Canned.overlays());
        assert!(Auth::Endpoint.overlays());
        assert!(!Auth::Inherited.overlays());
        assert_eq!(Auth::from_wire(Auth::Endpoint.as_wire()), None);
        for a in [Auth::Canned, Auth::Inherited] {
            assert_eq!(Auth::from_wire(a.as_wire()), Some(a));
        }
    }

    #[test]
    fn the_auth_failure_line_is_found_behind_a_banner_and_ahead_of_a_trace() {
        let line = "Error authenticating: IneligibleTierError: This client is no longer supported \
                    for Gemini Code Assist for individuals.";
        let stderr =
            format!("Loaded cached credentials.\n  {line}\n    at async main (x.js:1:1)\n");
        assert_eq!(auth_failure_line(&stderr).as_deref(), Some(line));
    }

    #[test]
    fn each_harnesss_spelling_of_an_auth_failure_is_recognised() {
        for line in [
            "Please set an Auth method in your /h/.gemini/settings.json or specify one of the \
             following environment variables before running: GEMINI_API_KEY",
            "API key not valid. Please pass a valid API key.",
            "Invalid API key · Please run /login",
            "Not logged in",
            "unexpected status 401 Unauthorized: Missing bearer",
            "HTTP 401",
            "error: (401) bad credentials",
        ] {
            assert_eq!(auth_failure_line(line).as_deref(), Some(line), "{line}");
        }
    }

    #[test]
    fn ordinary_stderr_carries_no_auth_failure() {
        for stderr in [
            "",
            "Loaded cached credentials.",
            "listening on 127.0.0.1:14010",
            "pid 4012 exited",
            "model gemini-2.5-flash-lite is no longer available",
        ] {
            assert_eq!(auth_failure_line(stderr), None, "{stderr}");
        }
    }

    #[test]
    fn a_very_long_auth_line_is_capped() {
        let long = format!("Error authenticating: {}", "x".repeat(2000));
        assert_eq!(auth_failure_line(&long).unwrap().chars().count(), 512);
    }
}
