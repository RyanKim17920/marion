//! **What a report must never carry**, taken out at one point.
//!
//! A report of a delegation tree is written to be shared: pasted into a pull request, attached to
//! an issue, sent to someone who was not there. Everything in it came from somewhere marion does
//! not control — a parent agent's instructions, a child's narrative, the arguments of every tool it
//! called, a check's stderr — and any of those can hold a key the agent read from a file, an
//! environment dump, or a path that names the operator's home. [`Scrubber::clean`] is the one
//! function every string of a report passes through before it is rendered, and the rendered text
//! passes through it a second time, so a field added later and forgotten is still caught.
//!
//! Three kinds of thing are removed, each for its own reason:
//!
//! * **Keys marion knows**: the credential behind every endpoint node in the tree, and the value of
//!   every environment variable whose name says it is a secret (`*_API_KEY`, `*_TOKEN`,
//!   `*_SECRET`, `*PASSWORD`, …). Matched exactly, by [`crate::endpoint::redact`] — the rule every
//!   other redaction in marion uses, so a key shorter than eight characters is left alone here as
//!   it is everywhere else.
//! * **Keys marion does not know, by their shape**: `sk-…`, `ghp_…`/`github_pat_…`, Slack's
//!   `xox?-…`, AWS's `AKIA…`, GitLab's `glpat-…`, a JWT, a `Bearer` credential, a run of 64 or
//!   more hex digits, and a PEM
//!   private-key block. Hand-matched and anchored at a word boundary. A 40-hex run is a git commit
//!   id — the thing a report exists to name — and is kept.
//! * **The operator's home directory**, as `~`, wherever it appears: it names the account.
//!
//! Over-redaction is the safe failure: a word that happens to equal a secret-named variable's value
//! is masked, and a reader loses a word rather than a key.

use marion_core::secret::Secret;

/// What every removed secret is replaced by — [`crate::endpoint::redact`]'s spelling.
pub const MASK: &str = "***";

/// The shortest value searched for by content, as [`crate::endpoint::redact`] decides it.
const MIN_SECRET: usize = 8;

/// Environment variable names, upper-cased, whose value is a secret by the name's own say-so.
const SECRET_ENV_SUFFIXES: &[&str] = &[
    "_API_KEY",
    "_TOKEN",
    "_SECRET",
    "PASSWORD",
    "_SECRET_KEY",
    "_ACCESS_KEY",
    "_PRIVATE_KEY",
];

/// The one scrubbing pass a report goes through. Holds the keys it searches for as [`Secret`]s, so
/// a `{:?}` of it prints none of them.
#[derive(Debug, Clone, Default)]
pub struct Scrubber {
    /// Longest first, so a key that contains another is replaced whole.
    keys: Vec<Secret>,
    /// `$HOME` without a trailing slash; `None` when unset or `/`.
    home: Option<String>,
}

impl Scrubber {
    /// A scrubber for `keys` (the credentials behind the tree's endpoint nodes), every value of
    /// `env` whose name says it is a secret, and `home`.
    pub fn new(
        keys: impl IntoIterator<Item = Secret>,
        env: impl IntoIterator<Item = (String, String)>,
        home: Option<String>,
    ) -> Self {
        let mut keys: Vec<Secret> = keys
            .into_iter()
            .chain(
                env.into_iter()
                    .filter(|(name, _)| is_secret_env_name(name))
                    .map(|(_, value)| Secret::new(value)),
            )
            .filter(|k| k.expose().len() >= MIN_SECRET)
            .collect();
        keys.sort_by(|a, b| {
            b.expose()
                .len()
                .cmp(&a.expose().len())
                .then_with(|| a.expose().cmp(b.expose()))
        });
        keys.dedup_by(|a, b| a.expose() == b.expose());
        let home = home
            .map(|h| h.trim_end_matches('/').to_string())
            .filter(|h| h.starts_with('/') && h.len() > 1);
        Scrubber { keys, home }
    }

    /// [`Self::new`] over this process's own environment and `$HOME`.
    pub fn from_process(keys: impl IntoIterator<Item = Secret>) -> Self {
        Self::new(keys, std::env::vars(), std::env::var("HOME").ok())
    }

    /// `text` with every known key, every secret-shaped run and the home directory taken out.
    pub fn clean(&self, text: &str) -> String {
        let mut out = text.to_string();
        for k in &self.keys {
            out = crate::endpoint::redact(&out, k.expose());
        }
        out = pem_blocks(&out);
        out = shapes(&out);
        match &self.home {
            Some(home) => home_as_tilde(&out, home),
            None => out,
        }
    }
}

/// Whether an environment variable named `name` holds a secret by its name.
fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_ENV_SUFFIXES.iter().any(|s| upper.ends_with(s))
}

/// Every PEM private-key block — `-----BEGIN … PRIVATE KEY-----` through its `END` line — as
/// [`MASK`]. A block with no end (a capped output cut it) is masked to the end of the text.
fn pem_blocks(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const TAIL: &str = "PRIVATE KEY-----";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(BEGIN) {
        let after = &rest[at + BEGIN.len()..];
        // The header's label is short and on one line: `RSA `, `EC `, `OPENSSH `, `ENCRYPTED `.
        let header_end = after
            .find(TAIL)
            .filter(|&i| i <= 32 && !after[..i].contains(['\n', '\r']));
        let Some(header_end) = header_end else {
            out.push_str(&rest[..at + BEGIN.len()]);
            rest = after;
            continue;
        };
        out.push_str(&rest[..at]);
        out.push_str(MASK);
        let body = &after[header_end + TAIL.len()..];
        rest = match body.find("-----END ") {
            Some(end) => {
                let from_end = &body[end..];
                match from_end.find(TAIL) {
                    Some(t) => &from_end[t + TAIL.len()..],
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// A byte that continues a word: a secret anchored at a boundary must not start inside one.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// How many bytes of `s` from its start satisfy `ok`.
fn run(s: &[u8], ok: impl Fn(u8) -> bool) -> usize {
    s.iter().take_while(|b| ok(**b)).count()
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// A base64url or token byte: letters, digits, `_` and `-`.
fn token_byte(b: u8) -> bool {
    is_word_byte(b)
}

fn alnum_or_underscore(b: u8) -> bool {
    alnum(b) || b == b'_'
}

fn upper_or_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

/// A vendor key known by its prefix: the prefix, which bytes its body is made of, and the shortest
/// body that makes it a key rather than a word that happens to start the same way.
struct Prefixed {
    prefix: &'static [u8],
    body: fn(u8) -> bool,
    min: usize,
}

const PREFIXED: &[Prefixed] = &[
    Prefixed {
        prefix: b"sk-",
        body: token_byte,
        min: 16,
    },
    Prefixed {
        prefix: b"ghp_",
        body: alnum,
        min: 20,
    },
    Prefixed {
        prefix: b"gho_",
        body: alnum,
        min: 20,
    },
    Prefixed {
        prefix: b"ghu_",
        body: alnum,
        min: 20,
    },
    Prefixed {
        prefix: b"ghs_",
        body: alnum,
        min: 20,
    },
    Prefixed {
        prefix: b"ghr_",
        body: alnum,
        min: 20,
    },
    Prefixed {
        prefix: b"github_pat_",
        body: alnum_or_underscore,
        min: 20,
    },
    Prefixed {
        prefix: b"AKIA",
        body: upper_or_digit,
        min: 16,
    },
    Prefixed {
        prefix: b"glpat-",
        body: token_byte,
        min: 20,
    },
];

/// The length of the secret starting at the front of `s`, and what it is replaced with, if one
/// does. Called only at a word boundary.
fn secret_at(s: &[u8]) -> Option<(usize, &'static str)> {
    for p in PREFIXED {
        if let Some(rest) = s.strip_prefix(p.prefix) {
            let n = run(rest, p.body);
            if n >= p.min {
                return Some((p.prefix.len() + n, MASK));
            }
        }
    }
    // Slack: `xox` + one of `abprs` + `-`.
    if s.len() > 4 && s.starts_with(b"xox") && b"abprs".contains(&s[3]) && s[4] == b'-' {
        let n = run(&s[5..], |b| alnum(b) || b == b'-');
        if n >= 10 {
            return Some((5 + n, MASK));
        }
    }
    // A JWT: `eyJ` (a base64url `{"`) and three dot-separated base64url segments.
    if let Some(rest) = s.strip_prefix(b"eyJ") {
        let head = run(rest, token_byte);
        if head >= 8 && rest.get(head) == Some(&b'.') {
            let body = run(&rest[head + 1..], token_byte);
            let at = head + 1 + body;
            if body > 0 && rest.get(at) == Some(&b'.') {
                let sig = run(&rest[at + 1..], token_byte);
                return Some((3 + at + 1 + sig, MASK));
            }
        }
    }
    // `Bearer <credential>`, any case: the word is kept so the reader sees what was there.
    if s.len() > 7 && s[..7].eq_ignore_ascii_case(b"bearer ") {
        let n = run(&s[7..], |b| {
            alnum(b) || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
        });
        if n >= MIN_SECRET {
            return Some((7 + n, "Bearer ***"));
        }
    }
    // 64 or more hex digits, the whole run: a sha-256 digest-shaped key, a raw private key. A
    // 40-hex commit id is shorter and stays.
    let n = run(s, |b| b.is_ascii_hexdigit());
    if n >= 64 && !s.get(n).is_some_and(|b| alnum(*b)) {
        return Some((n, MASK));
    }
    None
}

/// Every secret-shaped run in `text` as its replacement. Every shape begins with an ASCII byte, so
/// a slice at a match is always at a character boundary.
fn shapes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < bytes.len() {
        let at_boundary = i == 0 || !is_word_byte(bytes[i - 1]);
        if at_boundary && let Some((len, with)) = secret_at(&bytes[i..]) {
            out.push_str(&text[copied..i]);
            out.push_str(with);
            i += len;
            copied = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// `text` with `home` spelled `~` wherever it is a whole path prefix — followed by anything but a
/// letter, digit, `_` or `-` — so `/Users/fixture2` is not read as `~2`. A `.` ends it: a sentence's
/// full stop is far more common than a sibling named `fixture.old`, and `~.old` names nobody.
fn home_as_tilde(text: &str, home: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(home) {
        let next = rest.as_bytes().get(at + home.len()).copied();
        let whole = !next.is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        out.push_str(&rest[..at]);
        out.push_str(if whole { "~" } else { home });
        rest = &rest[at + home.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/fixture";

    fn scrubber() -> Scrubber {
        Scrubber::new(
            [Secret::new("memstore-key-0123456789")],
            [
                ("X_API_KEY".to_string(), "envkey-abcdefghijkl".to_string()),
                ("GITHUB_TOKEN".to_string(), "gh-env-token-value".to_string()),
                ("db_password".to_string(), "hunter2hunter2".to_string()),
                ("SHORT_TOKEN".to_string(), "abc".to_string()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ],
            Some(format!("{HOME}/")),
        )
    }

    /// **Every kind of secret the design names is removed**, each one planted in a sentence the way
    /// an agent would write it, and no part of any survives.
    #[test]
    fn every_secret_shape_and_known_key_is_masked() {
        let hex64 = "ab".repeat(32);
        let sentinels = [
            "memstore-key-0123456789",
            "envkey-abcdefghijkl",
            "gh-env-token-value",
            "hunter2hunter2",
            "sk-ant-api03-AAAAbbbbCCCCdddd1234",
            "sk-proj-0123456789abcdefXYZ",
            "ghp_0123456789abcdefghijABCDEFGHIJ",
            "github_pat_11ABCDEFG0123456789_abcdefghij",
            "xoxb-1234567890-abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "glpat-abcdefghij0123456789",
            "eyJhbGciOiJIUzI1NiJ9.payload.sig",
            &hex64,
        ];
        let s = scrubber();
        for planted in sentinels {
            for text in [
                format!("export K={planted}"),
                format!("curl -H 'Authorization: Bearer {planted}' x"),
                format!("{{\"key\":\"{planted}\"}}"),
                planted.to_string(),
            ] {
                let clean = s.clean(&text);
                assert!(
                    !clean.contains(planted),
                    "{planted} survived in {clean:?} (from {text:?})"
                );
            }
        }
        assert_eq!(
            s.clean("Authorization: bearer abc.def-ghi_jkl"),
            "Authorization: Bearer ***"
        );
    }

    /// A PEM private key is masked whole, header to footer, and one a cap cut short is masked to
    /// the end; a public certificate is not a secret and stays.
    #[test]
    fn a_pem_private_key_block_is_masked_whole() {
        let s = scrubber();
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\nzzzz\n\
                   -----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(s.clean(pem), "before\n***\nafter");
        let cut = "x -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAA";
        assert_eq!(s.clean(cut), "x ***");
        let escaped =
            r#"{\"k\":\"-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n\"}"#;
        assert_eq!(s.clean(escaped), r#"{\"k\":\"***\n\"}"#);
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(s.clean(cert), cert);
    }

    /// **What must stay stays**: a 40-hex commit id, words that merely contain a prefix, a key
    /// too short to search for, and a value of a variable whose name is not a secret's.
    #[test]
    fn commit_ids_ordinary_words_and_short_values_are_kept() {
        let s = scrubber();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for kept in [
            format!("landed {sha} on marion/t-1"),
            "risk-management-and-task-planning".to_string(),
            "the ask-for-review flag".to_string(),
            "abc is short".to_string(),
            "PATH=/usr/bin:/bin".to_string(),
            "Bearer tok".to_string(),
            "AKIA is a prefix".to_string(),
        ] {
            assert_eq!(s.clean(&kept), kept);
        }
        // 64 hex inside a longer word is not a separate run.
        let glued = format!("x{}", "a".repeat(64));
        assert_eq!(s.clean(&glued), glued);
    }

    /// **The home directory reads as `~`** wherever it is a whole prefix, and a sibling directory
    /// that merely starts with the same letters is left as it is.
    #[test]
    fn the_home_directory_reads_as_a_tilde() {
        let s = scrubber();
        assert_eq!(
            s.clean("cwd /Users/fixture/code/app; home /Users/fixture."),
            "cwd ~/code/app; home ~."
        );
        assert_eq!(s.clean("/Users/fixture2/x"), "/Users/fixture2/x");
        assert!(!s.clean("\"/Users/fixture\"").contains(HOME));
        // An unset or root home is nothing to replace.
        let none = Scrubber::new([], [], Some("/".into()));
        assert_eq!(none.clean("/Users/fixture/x"), "/Users/fixture/x");
    }

    /// Cleaning is idempotent, so the backstop pass over rendered output changes nothing the first
    /// pass already cleaned; and a scrubber's `Debug` names none of its keys.
    #[test]
    fn cleaning_twice_is_cleaning_once_and_debug_prints_no_key() {
        let s = scrubber();
        let text = format!(
            "{HOME}/x sk-ant-api03-AAAAbbbbCCCCdddd1234 Bearer abcdefghijk {} memstore-key-0123456789",
            "f".repeat(64)
        );
        let once = s.clean(&text);
        assert_eq!(s.clean(&once), once);
        let debug = format!("{s:?}");
        for key in ["memstore-key", "envkey", "hunter2"] {
            assert!(!debug.contains(key), "{debug}");
        }
    }

    #[test]
    fn a_secret_env_name_is_read_by_its_suffix_in_any_case() {
        for name in [
            "OPENAI_API_KEY",
            "gh_token",
            "APP_SECRET",
            "PGPASSWORD",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(is_secret_env_name(name), "{name}");
        }
        for name in ["PATH", "HOME", "TOKENIZER", "KEYBOARD", "SECRETARY"] {
            assert!(!is_secret_env_name(name), "{name}");
        }
    }
}
