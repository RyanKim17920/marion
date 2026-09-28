use marion_core::contract::FailureCause;
use serde_json::Value;

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
    // OpenAI's own 401 sentence, which OpenAI-compatible providers repeat and harnesses pass on
    // with no status beside it (S37: opencode's ACP agent, `Internal error: Incorrect API key
    // provided`).
    "incorrect api key",
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
    AUTH_FAILURE_MARKERS.iter().any(|m| lower.contains(m)) || has_status(&lower, "401")
}

/// Phrases that mean **the account's usage window is spent**, lowercased. Measured strings:
/// claude 2.1.283 renders `You've hit your ${h} limit` with `h` = `session` (five-hour window) or
/// `weekly` (seven-day), and tags the assistant frame `"error":"rate_limit"`; codex 0.155.1 says
/// `You've hit your usage limit.` and tags the error `usage_limit_exceeded` /
/// `usage_limit_reached` (all read from the installed binaries' strings).
///
/// **A usage limit is never a reason to switch accounts.** This list exists so the limit is
/// *reported*: [`failure_cause`] ranks it above auth, so a limit line can never be mistaken for the
/// expired login that alone licenses a profile failover.
const USAGE_LIMIT_MARKERS: &[&str] = &[
    "hit your session limit",
    "hit your weekly limit",
    "hit your usage limit",
    "usage_limit_exceeded",
    "usage_limit_reached",
    // app-server's `codexErrorInfo` spells the same tag in camel case (S36 schema `CodexErrorInfo`).
    "usagelimitexceeded",
    "\"error\":\"rate_limit\"",
];

/// Phrases that mean **the vendor is failing or unreachable, not the account**, lowercased:
/// Anthropic's `overloaded_error` and codex's `server_overloaded` (both carry `overloaded`), the
/// HTTP reason phrases, and the connection errors of every OpenAI- and Anthropic-compatible client.
/// A bare 5xx is matched as a token of its own ([`OUTAGE_STATUSES`]).
const OUTAGE_MARKERS: &[&str] = &[
    "overloaded",
    // codex 0.155.1's words for a provider 5xx (S37 `codex-0.155.1/p-errors-500.jsonl`): no status
    // on the frame at all, only `We’re currently experiencing high demand, which may cause
    // temporary errors.`
    "experiencing high demand",
    "service unavailable",
    "internal server error",
    "bad gateway",
    "gateway timeout",
    "connection refused",
    "econnrefused",
    "connection reset",
    "econnreset",
    "error sending request",
    "failed to connect",
    "could not connect",
    "dns error",
    "enotfound",
];

/// Phrases that mean an API provider **rate-limited** the request, lowercased; a bare `429` is
/// matched as a token of its own. Read only for an [`Billing::ApiKey`] run: on a subscription the
/// same words are prose about the account's window, and the window has its own sentences
/// ([`USAGE_LIMIT_MARKERS`]).
const RATE_LIMIT_MARKERS: &[&str] = &["rate limit", "rate_limit", "ratelimit", "too many requests"];

/// **Whose account a run spends**, which is what a 429 means. On an API key it is a provider's
/// per-key rate limit, which another key may not share; on a subscription (the operator's own
/// login, a profile) it is the account's usage window, which no relaunch works around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Billing {
    ApiKey,
    Subscription,
}

/// The HTTP statuses read as an outage when they stand alone on an error line.
const OUTAGE_STATUSES: &[&str] = &["500", "502", "503", "504", "529"];

/// **Why the run failed**, read from `stderr` and from the error-shaped frames of `stdout`, or
/// `None` where no line says — the one classifier behind both relaunch policies (an endpoint's
/// credential rotation and a profile failover) and every contract's `failure_cause`. Precedence
/// **usage limit or rate limit > auth > outage**, over the whole input: a run that printed a limit
/// line and an auth-looking one is a limit. `billing` decides what a limit is (see [`Billing`]):
/// the account's window on a subscription, and on an API key the provider's rate limit unless the
/// line names a usage window outright.
///
/// Only error-shaped stdout frames are read ([`error_shaped`]) — never an assistant's prose, a tool
/// result or a narrative, where a child writing *about* a 429 would otherwise read as having hit
/// one. `stderr` is read whole, as [`auth_failure_line`] reads it.
pub fn failure_cause(stderr: &str, stdout: &str, billing: Billing) -> Option<FailureCause> {
    let mut lines = stderr_candidates(stderr);
    lines.extend(stdout.lines().filter_map(|l| {
        let frame: Value = serde_json::from_str(l.trim()).ok()?;
        error_shaped(&frame).then(|| Candidate {
            raw: l.trim().to_string(),
            frame: Some(frame),
            reported: None,
        })
    }));
    classify(&lines, billing)
}

/// **[`failure_cause`] over the provider errors a row's own grammar read off the stream**
/// ([`crate::grammar::error_reports`]) instead of every error-shaped frame — the reading a harness
/// with a stream grammar is classified by (`HarnessAdapter::failure_cause`). Each report's line is
/// what is matched, never its frame's whole text, so a number elsewhere in the frame (a duration,
/// a token count) is not read as a status; the frame is kept for what only it carries.
///
/// A report is a structured statement of a provider error, not prose, so on a subscription its
/// rate-limit words are the account's window as surely as a 429 is — claude's `rate_limit` tag.
pub fn reported_failure_cause(
    stderr: &str,
    reports: &[crate::grammar::ErrorReport],
    billing: Billing,
) -> Option<FailureCause> {
    let mut lines = stderr_candidates(stderr);
    lines.extend(reports.iter().map(|r| Candidate {
        raw: r.line.clone(),
        frame: Some(r.frame.clone()),
        reported: r.words.clone().or_else(|| Some(r.line.clone())),
    }));
    classify(&lines, billing)
}

/// **A provider's refusal of the credential, among `reports`**: the first report that the one
/// classifier reads as `Auth` **and** that carries the provider's status, a standalone 401 or 403
/// — what no retry heals, and so what ends a run early. The words alone (`unauthorized`) are not
/// enough here: codex reports an operator's MCP server refusing it on the same error frame.
pub fn refused_credential(reports: &[crate::grammar::ErrorReport]) -> Option<String> {
    reports.iter().find_map(|r| {
        let lower = r.line.to_ascii_lowercase();
        let provider = has_status(&lower, "401") || has_status(&lower, "403");
        match reported_failure_cause("", std::slice::from_ref(r), Billing::Subscription)? {
            FailureCause::Auth { line } if provider => Some(line),
            _ => None,
        }
    })
}

fn stderr_candidates(stderr: &str) -> Vec<Candidate> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Candidate {
            raw: l.to_string(),
            frame: None,
            reported: None,
        })
        .collect()
}

/// The one ranking, over whatever lines were read: limit > auth > outage (module doc).
fn classify(lines: &[Candidate], billing: Billing) -> Option<FailureCause> {
    // A sentence beats a bare structural signal: the rejected `rate_limit_event` usually rides
    // beside the frame that says it in words, and the words are what a notice quotes.
    if billing == Billing::ApiKey
        && lines.iter().all(|c| !c.names_a_window())
        && let Some(c) = lines.iter().find(|c| c.is_rate_limit())
    {
        return Some(FailureCause::RateLimit {
            line: c.words(RATE_LIMIT_MARKERS),
        });
    }
    let limit = lines
        .iter()
        .find(|c| c.is_limit_text())
        .or_else(|| lines.iter().find(|c| c.is_limit()));
    if let Some(c) = limit {
        // The reset rides the rate-limit event beside the sentence on claude, so any frame of the
        // run that states one is read, not only the line that matched.
        let resets = c.frame.as_ref().and_then(resets_at).or_else(|| {
            lines
                .iter()
                .filter_map(|c| c.frame.as_ref())
                .find_map(resets_at)
        });
        return Some(FailureCause::UsageLimit {
            line: c.words(USAGE_LIMIT_MARKERS),
            resets_at: resets,
        });
    }
    if let Some(c) = lines.iter().find(|c| c.is_auth()) {
        return Some(FailureCause::Auth {
            line: c.words(AUTH_FAILURE_MARKERS),
        });
    }
    lines
        .iter()
        .find(|c| c.is_outage())
        .map(|c| FailureCause::Outage {
            line: c.words(OUTAGE_MARKERS),
        })
}

/// One line [`failure_cause`] reads, with its frame where it was JSON.
struct Candidate {
    raw: String,
    frame: Option<Value>,
    /// A row's grammar read it as a provider error report ([`reported_failure_cause`]): `raw` is
    /// then the report's line, and this the sentence a notice quotes (the harness's words, else
    /// the line).
    reported: Option<String>,
}

impl Candidate {
    fn is_limit_text(&self) -> bool {
        let lower = self.raw.to_ascii_lowercase();
        USAGE_LIMIT_MARKERS.iter().any(|m| lower.contains(m))
            || has_status(&lower, "429")
            || (self.reported.is_some() && RATE_LIMIT_MARKERS.iter().any(|m| lower.contains(m)))
    }

    fn is_limit(&self) -> bool {
        self.is_limit_text() || self.frame.as_ref().is_some_and(rejected_rate_limit)
    }

    /// A usage window named in words: the account's own limit, on any billing.
    fn names_a_window(&self) -> bool {
        let lower = self.raw.to_ascii_lowercase();
        USAGE_LIMIT_MARKERS.iter().any(|m| lower.contains(m))
    }

    /// An API provider's rate limit: its words, a standalone 429, or a refused rate-limit event.
    fn is_rate_limit(&self) -> bool {
        let lower = self.raw.to_ascii_lowercase();
        RATE_LIMIT_MARKERS.iter().any(|m| lower.contains(m))
            || has_status(&lower, "429")
            || self.frame.as_ref().is_some_and(rejected_rate_limit)
    }

    /// A login missing, expired or refused — or a key refused a resource (a standalone 403,
    /// `forbidden`, Anthropic's `permission_error`), which on either billing is the credential's.
    fn is_auth(&self) -> bool {
        let lower = self.raw.to_ascii_lowercase();
        is_auth_failure_line(&self.raw)
            || has_status(&lower, "403")
            || lower.contains("forbidden")
            || lower.contains("permission_error")
    }

    fn is_outage(&self) -> bool {
        let lower = self.raw.to_ascii_lowercase();
        OUTAGE_MARKERS.iter().any(|m| lower.contains(m))
            || OUTAGE_STATUSES.iter().any(|s| has_status(&lower, s))
    }

    /// The sentence a person reads: for a frame, its first string that carries one of `markers`
    /// (else its own message), for a plain line the line — trimmed and capped either way.
    fn words(&self, markers: &[&str]) -> String {
        if let Some(said) = &self.reported {
            return said.trim().chars().take(512).collect();
        }
        let text = self
            .frame
            .as_ref()
            .and_then(|f| {
                message(f)
                    .filter(|m| carries(m, markers))
                    .or_else(|| marked_string(f, markers))
                    .or_else(|| message(f))
            })
            .unwrap_or_else(|| self.raw.clone());
        text.trim().chars().take(512).collect()
    }
}

fn carries(s: &str, markers: &[&str]) -> bool {
    let lower = s.to_ascii_lowercase();
    markers.iter().any(|m| lower.contains(m))
}

/// A stdout frame that is a harness's **error report** rather than conversation: a `type` that
/// names an error or a failure, a `result` flagged `is_error`, a top-level `error` member, or a
/// `rate_limit_event`. Nothing an assistant or a tool wrote qualifies.
fn error_shaped(frame: &Value) -> bool {
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    kind.contains("error")
        || kind.contains("failed")
        || kind == "rate_limit_event"
        || frame.get("is_error").and_then(Value::as_bool) == Some(true)
        || frame.get("error").is_some_and(|e| !e.is_null())
}

/// Claude's `rate_limit_event` whose window refused the request (`status: "rejected"`); the
/// `allowed` and `allowed_warning` events are readings, not failures.
fn rejected_rate_limit(frame: &Value) -> bool {
    frame.get("type").and_then(Value::as_str) == Some("rate_limit_event")
        && frame
            .pointer("/rate_limit_info/status")
            .and_then(Value::as_str)
            == Some("rejected")
}

/// The reset instant a frame states, in unix seconds: the first `resetsAt` (claude) or
/// `resets_at` (codex) number anywhere in it.
fn resets_at(frame: &Value) -> Option<u64> {
    match frame {
        Value::Object(map) => ["resetsAt", "resets_at"]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_u64))
            .or_else(|| map.values().find_map(resets_at)),
        Value::Array(items) => items.iter().find_map(resets_at),
        _ => None,
    }
}

/// The first string in `v` that carries one of `markers`, depth first.
fn marked_string(v: &Value, markers: &[&str]) -> Option<String> {
    match v {
        Value::String(s) => carries(s, markers).then(|| s.clone()),
        Value::Object(map) => map.values().find_map(|v| marked_string(v, markers)),
        Value::Array(items) => items.iter().find_map(|v| marked_string(v, markers)),
        _ => None,
    }
}

/// A frame's own message, where it names one at a conventional place.
fn message(frame: &Value) -> Option<String> {
    ["/message", "/result", "/error/message", "/error"]
        .iter()
        .find_map(|p| frame.pointer(p).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// `status` as a token of its own — not the tail of a port, a pid or an id (`14290`, `pid 54290`).
fn has_status(lower: &str, status: &str) -> bool {
    lower.match_indices(status).any(|(i, _)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + status.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// **The words a limit notice quotes for when the window reopens**: the harness's own `resets …`
/// clause where its line carries one (claude's `· resets 7:50pm`), else the stated instant, else
/// nothing.
pub fn resets_phrase(line: &str, resets_at: Option<u64>) -> Option<String> {
    if let Some(i) = line.to_ascii_lowercase().find("resets ") {
        return Some(
            line[i..]
                .trim()
                .trim_end_matches(['.', ')', '('])
                .trim()
                .to_string(),
        );
    }
    resets_at.map(|secs| {
        let when = marion_core::encoding::SystemTime::from_unix_millis(secs.saturating_mul(1000));
        let stamp = serde_json::to_value(when)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| secs.to_string());
        format!("resets {stamp}")
    })
}

/// Which window a limit line names — `session`, `weekly` — or the generic `usage`.
pub fn limit_window(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("weekly") || lower.contains("seven_day") {
        "weekly"
    } else if lower.contains("session") || lower.contains("five_hour") {
        "session"
    } else {
        "usage"
    }
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

    // ---- endpoint credential failover: rate limit > auth > outage ----

    /// **Each measured or documented spelling of the three rotatable failures**: codex 0.155.1's
    /// `turn.failed` messages for 429 and 401 (measured against the canned provider), a 403, the
    /// vendors' 5xx and overload words, and a connection that never reached the provider.
    #[test]
    fn each_rotatable_endpoint_failure_is_classified() {
        #[derive(Debug, PartialEq)]
        enum Kind {
            RateLimit,
            Auth,
            Outage,
        }
        use Kind::{Auth, Outage, RateLimit};
        let kind = |text: &str| match failure_cause(text, "", Billing::ApiKey) {
            Some(FailureCause::RateLimit { .. }) => Some(RateLimit),
            Some(FailureCause::Auth { .. }) => Some(Auth),
            Some(FailureCause::Outage { .. }) => Some(Outage),
            _ => None,
        };
        for (text, want) in [
            (
                "exceeded retry limit, last status: 429 Too Many Requests",
                RateLimit,
            ),
            ("Rate limit reached for requests", RateLimit),
            ("{\"type\":\"rate_limit_error\"}", RateLimit),
            (
                "unexpected status 401 Unauthorized: bad key, url: http://127.0.0.1:1/v1/responses",
                Auth,
            ),
            ("HTTP 403 Forbidden", Auth),
            ("Invalid API key provided", Auth),
            ("unexpected status 503 Service Unavailable", Outage),
            ("{\"type\":\"overloaded_error\"}", Outage),
            (
                "error sending request for url (http://127.0.0.1:9/v1): Connection refused",
                Outage,
            ),
            ("fetch failed: ECONNREFUSED 127.0.0.1:9", Outage),
        ] {
            assert_eq!(kind(text), Some(want), "{text}");
        }
        // A limit line beside an auth-looking one is a limit.
        assert_eq!(
            kind("401 retry\nlast status: 429 Too Many Requests"),
            Some(RateLimit)
        );
        // The same 429 on a subscription is the account's window, which no relaunch works around.
        assert!(matches!(
            failure_cause(
                "last status: 429 Too Many Requests",
                "",
                Billing::Subscription
            ),
            Some(FailureCause::UsageLimit { .. })
        ));
    }

    #[test]
    fn ordinary_failures_are_not_a_reason_to_rotate() {
        for text in [
            "",
            "Model metadata for `m` not found. Defaulting to fallback metadata",
            "listening on 127.0.0.1:14290",
            "pid 5031 exited",
            "the child could not find src/main.rs",
            "context length exceeded (400 Bad Request)",
        ] {
            assert_eq!(failure_cause(text, "", Billing::ApiKey), None, "{text}");
        }
    }

    #[test]
    fn a_very_long_auth_line_is_capped() {
        let long = format!("Error authenticating: {}", "x".repeat(2000));
        assert_eq!(auth_failure_line(&long).unwrap().chars().count(), 512);
    }

    // ---- failure causes: usage limit > auth > outage ----

    fn limit(line: &str, resets_at: Option<u64>) -> Option<FailureCause> {
        Some(FailureCause::UsageLimit {
            line: line.into(),
            resets_at,
        })
    }

    /// The strings the installed binaries carry: claude 2.1.283's `You've hit your ${h} limit`
    /// for its five-hour and seven-day windows, codex 0.155.1's `You've hit your usage limit.`
    #[test]
    fn each_measured_usage_limit_sentence_is_a_usage_limit() {
        for line in [
            "You've hit your session limit · resets 7:50pm",
            "You've hit your weekly limit · resets Oct 3, 9am",
            "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits",
            "error: 429 Too Many Requests",
        ] {
            assert_eq!(
                failure_cause(line, "", Billing::Subscription),
                limit(line, None),
                "{line}"
            );
        }
    }

    /// Claude's stream: a `rate_limit_event` that refused the window carries the reset instant, and
    /// the assistant frame it rode with is tagged `"error":"rate_limit"`.
    #[test]
    fn a_rejected_rate_limit_event_is_a_usage_limit_with_its_reset() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            "\n",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790538000,"rateLimitType":"five_hour"}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your session limit · resets 7:50pm"}"#,
        );
        assert_eq!(
            failure_cause("", stdout, Billing::Subscription),
            limit(
                "You've hit your session limit · resets 7:50pm",
                Some(1_790_538_000)
            )
        );
        let tagged = r#"{"type":"assistant","error":"rate_limit","message":{"content":[]}}"#;
        assert!(matches!(
            failure_cause("", tagged, Billing::Subscription),
            Some(FailureCause::UsageLimit { .. })
        ));
    }

    /// Codex's JSONL error: the tag `usage_limit_exceeded` and its `resets_at`.
    #[test]
    fn a_codex_usage_limit_error_frame_carries_its_reset() {
        let stdout = r#"{"type":"error","message":"You've hit your usage limit.","codex_error_info":"usage_limit_exceeded","resets_at":1790541600}"#;
        assert_eq!(
            failure_cause("", stdout, Billing::Subscription),
            limit("You've hit your usage limit.", Some(1_790_541_600))
        );
    }

    /// Allowed readings are not failures, and prose about limits — in stderr or in a model's
    /// words — is not a limit.
    #[test]
    fn readings_prose_and_ports_are_not_usage_limits() {
        let allowed = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","isUsingOverage":false}}"#;
        assert_eq!(failure_cause("", allowed, Billing::Subscription), None);
        let narrative = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"You've hit your session limit is what users see on a 429"}]}}"#;
        assert_eq!(
            failure_cause("", narrative, Billing::Subscription),
            None,
            "an assistant's prose"
        );
        for stderr in [
            "retrying after a rate limit",
            "listening on 127.0.0.1:14290",
            "pid 54290 exited",
            "rate_limit_event received",
        ] {
            assert_eq!(
                failure_cause(stderr, "", Billing::Subscription),
                None,
                "{stderr}"
            );
        }
    }

    #[test]
    fn an_auth_failure_is_classified_from_stderr_or_an_error_frame() {
        assert_eq!(
            failure_cause("Not logged in", "", Billing::Subscription),
            Some(FailureCause::Auth {
                line: "Not logged in".into()
            })
        );
        let frame = r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}"#;
        assert_eq!(
            failure_cause("", frame, Billing::Subscription),
            Some(FailureCause::Auth {
                line: "Invalid API key · Please run /login".into()
            })
        );
    }

    #[test]
    fn an_outage_is_the_vendors_overload_or_a_standalone_5xx() {
        for (stderr, stdout) in [
            (
                "",
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ),
            ("", r#"{"type":"error","message":"server_overloaded"}"#),
            ("API Error: 529 Overloaded", ""),
            ("unexpected status 503 Service Unavailable", ""),
        ] {
            assert!(
                matches!(
                    failure_cause(stderr, stdout, Billing::Subscription),
                    Some(FailureCause::Outage { .. })
                ),
                "{stderr}{stdout}"
            );
        }
        assert_eq!(
            failure_cause("listening on :5030", "", Billing::Subscription),
            None
        );
    }

    /// Claude's stream read through its own grammar's error rules, as a claude node is classified.
    fn through_claude(stdout: &str, billing: Billing) -> Option<FailureCause> {
        let reports = crate::grammar::error_reports(&crate::claude_code::STREAM, stdout);
        reported_failure_cause("", &reports, billing)
    }

    /// **The account-limit shapes survive the row's reading**: a refused usage window with its
    /// reset, the assistant frame tagged `rate_limit` (words or none), on a subscription — while an
    /// assistant's prose about a limit is no report at all, and a retried 429 on an API key is the
    /// provider's rate limit, not the account's window (the heuristic read claude's `rate_limit`
    /// retry code as a usage window on every billing).
    #[test]
    fn claudes_limit_frames_classify_through_its_error_rules() {
        let stdout = concat!(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790538000,"rateLimitType":"five_hour"}}"#,
            "\n",
            r#"{"type":"assistant","error":"rate_limit","message":{"content":[{"type":"text","text":"You've hit your session limit · resets 7:50pm"}]}}"#,
        );
        assert_eq!(
            through_claude(stdout, Billing::Subscription),
            limit(
                "You've hit your session limit · resets 7:50pm",
                Some(1_790_538_000)
            )
        );
        let tagged = r#"{"type":"assistant","error":"rate_limit","message":{"content":[]}}"#;
        assert!(matches!(
            through_claude(tagged, Billing::Subscription),
            Some(FailureCause::UsageLimit { .. })
        ));
        let prose = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"You've hit your session limit is what users see on a 429"}]}}"#;
        assert_eq!(through_claude(prose, Billing::Subscription), None);
        let retry = r#"{"type":"system","subtype":"api_retry","attempt":1,"error":"rate_limit","error_status":429,"retry_delay_ms":584}"#;
        assert_eq!(
            through_claude(retry, Billing::ApiKey),
            Some(FailureCause::RateLimit {
                line: "HTTP 429 rate_limit".into()
            })
        );
        assert!(matches!(
            through_claude(retry, Billing::Subscription),
            Some(FailureCause::UsageLimit { .. })
        ));
        let refused = r#"{"type":"system","subtype":"api_retry","attempt":1,"error":"authentication_failed","error_status":401}"#;
        assert_eq!(
            through_claude(refused, Billing::Subscription),
            Some(FailureCause::Auth {
                line: "HTTP 401 authentication_failed".into()
            })
        );
    }

    /// A limit outranks an auth line in the same run, and an auth line outranks an outage: the
    /// ranking is what keeps a limit from ever reaching the failover an auth failure may take.
    #[test]
    fn the_precedence_is_limit_then_auth_then_outage() {
        let both = "Not logged in\nYou've hit your session limit · resets 7:50pm\n503";
        assert!(matches!(
            failure_cause(both, "", Billing::Subscription),
            Some(FailureCause::UsageLimit { .. })
        ));
        assert!(matches!(
            failure_cause(
                "503 Service Unavailable\nInvalid API key",
                "",
                Billing::Subscription
            ),
            Some(FailureCause::Auth { .. })
        ));
    }

    #[test]
    fn the_reset_phrase_is_the_harnesss_own_clause_else_the_stated_instant() {
        assert_eq!(
            resets_phrase("You've hit your session limit · resets 7:50pm", None).as_deref(),
            Some("resets 7:50pm")
        );
        assert_eq!(
            resets_phrase("usage_limit_exceeded", Some(0)).as_deref(),
            Some("resets 1970-01-01T00:00:00.000Z")
        );
        assert_eq!(resets_phrase("hit your usage limit", None), None);
        assert_eq!(limit_window("You've hit your weekly limit"), "weekly");
        assert_eq!(limit_window("You've hit your session limit"), "session");
        assert_eq!(limit_window("usage_limit_exceeded"), "usage");
    }
}
