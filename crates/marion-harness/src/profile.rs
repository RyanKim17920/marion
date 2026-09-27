//! **Profiles, harness side**: how a row points its harness at a directory the operator logged
//! into themselves — one account per directory, several directories per harness.
//!
//! A profile is nothing but a directory and the variable that names it. marion never logs in,
//! never copies, links or reads a credential: the operator runs the harness's own login once with
//! the variable set, and from then on a launch that selects the profile carries the same variable.
//! What differs per harness — which variable, which variables must be *cleared* beside it, how to
//! ask the harness whether the directory is logged in, which non-secret files may be shared from
//! the default directory — is the row's [`ProfileCarrier`], so the mechanism names no harness.
//!
//! The carrier applies **under [`Auth::Inherited`] only**. A canned launch already relocates the
//! harness's home onto marion's own directory for isolation, and a profile there would be a second
//! answer to the same question.

use crate::auth::Auth;
use crate::spec::Fields;

/// How one harness is pointed at a profile directory, stated as data. Every row states one or
/// `None`, and the sweep `every_row_states_its_profile_carrier_or_why_not` lists each `None` with
/// the measurement behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileCarrier {
    /// The variable that names the directory: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, … Set to the
    /// directory **exactly as stored** — claude keys its keychain entry on a hash of the exported
    /// string, so a spelling marion normalised would be a different, logged-out account.
    pub env: &'static str,
    /// Variables that would override [`Self::env`]'s choice if the operator's environment carried
    /// them, and are therefore removed from a profiled launch — claude's
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` names the secure store outright when it is defined.
    pub clear: &'static [&'static str],
    /// How to ask the harness, with the profile's variable set, whether the directory is logged
    /// in. Read-only on every row: none of these starts a login.
    pub status: Status,
    /// The arguments after the program that sign in, **printed for the operator and never run**:
    /// `marion profile add` shows `<env>=<dir> <program> <login_hint>` and stops. Empty where the
    /// harness signs in from its own first screen.
    pub login_hint: &'static str,
    /// The directory the harness uses when [`Self::env`] is unset, relative to `$HOME` — where
    /// [`Self::shared`] is linked from.
    pub home_default: &'static str,
    /// Non-secret files and directories that may be shared from the harness's default directory
    /// into a new profile (settings, instructions, skills). **Never** a credential or an account
    /// file: the sweep refuses any name that looks like one.
    pub shared: &'static [&'static str],
    /// Where each fact above was measured.
    pub note: &'static str,
}

/// A read-only probe of whether a profile directory is logged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Run the program with `argv`; the JSON object on stdout carries `key: true` when logged in
    /// (claude's `auth status --json` → `loggedIn`).
    JsonBool {
        argv: &'static [&'static str],
        key: &'static str,
    },
    /// Run the program with `argv`; it is logged in unless its output contains `text` (codex's
    /// `login status` → `Not logged in`).
    TextAbsent {
        argv: &'static [&'static str],
        text: &'static str,
    },
    /// Logged in when this file exists under the profile directory. For a harness with no status
    /// command; existence only — the file is never opened.
    FileExists(&'static str),
}

/// **The profile's part of a launch's environment**: `env` gains the carrier's variable set to the
/// profile directory (replacing any value a row or the operator gave it), loses the carrier's
/// `clear` variables, and the names to remove from the inherited environment are returned.
///
/// A no-op — `env` byte-identical, nothing returned — without a profile directory, without a
/// carrier, or under [`Auth::Canned`].
pub fn apply(
    carrier: Option<&ProfileCarrier>,
    f: &Fields,
    env: &mut Vec<(String, String)>,
) -> Vec<String> {
    let (Some(carrier), Some(dir), Auth::Inherited) = (carrier, f.profile_dir.as_ref(), f.auth)
    else {
        return Vec::new();
    };
    env.retain(|(k, _)| k != carrier.env && !carrier.clear.contains(&k.as_str()));
    env.push((carrier.env.to_string(), dir.to_string_lossy().into_owned()));
    carrier.clear.iter().map(|k| k.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const CARRIER: ProfileCarrier = ProfileCarrier {
        env: "X_HOME",
        clear: &["X_STORE"],
        status: Status::FileExists("creds"),
        login_hint: "login",
        home_default: ".x",
        shared: &[],
        note: "test",
    };

    fn fields(auth: Auth, dir: Option<&str>) -> Fields {
        Fields {
            auth,
            profile_dir: dir.map(PathBuf::from),
            ..Fields::default()
        }
    }

    #[test]
    fn a_profiled_inherited_launch_sets_the_variable_and_clears_the_overrides() {
        let mut env = vec![
            ("X_HOME".into(), "/row".into()),
            ("X_STORE".into(), "/other".into()),
            ("KEEP".into(), "1".into()),
        ];
        let removed = apply(
            Some(&CARRIER),
            &fields(Auth::Inherited, Some("~/p/work")),
            &mut env,
        );
        assert_eq!(
            env,
            vec![
                ("KEEP".to_string(), "1".to_string()),
                ("X_HOME".to_string(), "~/p/work".to_string()),
            ],
            "the directory exactly as stored, never normalised"
        );
        assert_eq!(removed, vec!["X_STORE".to_string()]);
    }

    #[test]
    fn without_a_profile_a_carrier_or_under_canned_nothing_changes() {
        let before = vec![("X_STORE".to_string(), "/s".to_string())];
        for (carrier, f) in [
            (Some(&CARRIER), fields(Auth::Inherited, None)),
            (None, fields(Auth::Inherited, Some("/p"))),
            (Some(&CARRIER), fields(Auth::Canned, Some("/p"))),
        ] {
            let mut env = before.clone();
            assert!(apply(carrier, &f, &mut env).is_empty());
            assert_eq!(env, before);
        }
    }

    // ---- the rows ----

    use crate::adapter::harness_spec;
    use crate::spec::{Shape, render};
    use marion_core::harness::Harness;

    /// Every row without a carrier, and the measurement behind the absence. A row that gains or
    /// loses one must move here too, so an absence is always a statement and never an oversight.
    const NO_CARRIER: &[(Harness, &str)] = &[
        (
            Harness::Copilot,
            "1.0.83: a fresh COPILOT_HOME still authenticated, so the login is not per home",
        ),
        (Harness::Goose, "unmeasured"),
        (Harness::Cline, "unmeasured"),
        (Harness::Qwen, "unmeasured"),
        (
            Harness::Acp,
            "one row, many agents, each with its own store",
        ),
    ];

    /// Names a carrier may never share: a credential or an account file, in any spelling.
    const SECRET_LOOKING: &[&str] = &[
        "auth",
        "cred",
        "token",
        "oauth",
        "apikey",
        "api_key",
        "secret",
        ".claude.json",
    ];

    #[test]
    fn every_row_states_its_profile_carrier_or_why_not() {
        for h in Harness::ALL {
            let row = harness_spec(h);
            let listed = NO_CARRIER.iter().find(|(n, _)| *n == h);
            match (&row.profile, listed) {
                (None, Some((_, why))) => assert!(!why.is_empty()),
                (Some(c), None) => {
                    assert!(!c.env.is_empty() && !c.note.is_empty(), "{h}");
                    for shared in c.shared {
                        let lower = shared.to_ascii_lowercase();
                        assert!(
                            !SECRET_LOOKING.iter().any(|s| lower.contains(s)),
                            "{h} shares {shared}, which looks like a credential"
                        );
                    }
                    if let Status::JsonBool { argv, .. } | Status::TextAbsent { argv, .. } =
                        c.status
                    {
                        assert_ne!(argv, ["login"], "{h}'s status probe must not be a login");
                        assert!(argv.contains(&"status"), "{h}'s probe is a status command");
                    }
                }
                (None, None) => panic!("{h} has no carrier and NO_CARRIER does not say why"),
                (Some(_), Some(_)) => panic!("{h} has a carrier and is listed as having none"),
            }
        }
    }

    fn row_fields(auth: Auth, dir: Option<&str>) -> Fields {
        Fields {
            program: Some("agent".into()),
            model: Some("m".into()),
            config_dir: "/state/cfg".into(),
            cwd: "/wt".into(),
            ..fields(auth, dir)
        }
    }

    /// Without a profile every row renders exactly what it rendered before profiles existed, and
    /// a canned launch ignores a profile entirely: the canned rows keep their own isolation.
    #[test]
    fn no_profile_or_canned_auth_renders_byte_identically_on_every_row() {
        for h in Harness::ALL {
            let row = harness_spec(h);
            for auth in [Auth::Canned, Auth::Inherited] {
                let Ok(plain) = render(row, Shape::Headless, &row_fields(auth, None)) else {
                    continue;
                };
                assert!(plain.env_remove.is_empty(), "{h} {auth:?}");
                if auth == Auth::Canned {
                    let profiled = render(row, Shape::Headless, &row_fields(auth, Some("/p")))
                        .expect("rendered above");
                    assert_eq!(profiled, plain, "{h}: a canned launch carries no profile");
                }
            }
        }
    }

    fn values<'a>(env: &'a [(String, String)], key: &str) -> Vec<&'a str> {
        env.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    #[test]
    fn a_profiled_claude_launch_names_the_directory_and_clears_the_secure_store() {
        let inv = render(
            harness_spec(Harness::ClaudeCode),
            Shape::Headless,
            &row_fields(Auth::Inherited, Some("/profiles/claude-code/work")),
        )
        .unwrap();
        let env = &inv.env;
        assert_eq!(
            values(env, "CLAUDE_CONFIG_DIR"),
            ["/profiles/claude-code/work"]
        );
        assert!(values(env, "CLAUDE_SECURESTORAGE_CONFIG_DIR").is_empty());
        assert_eq!(inv.env_remove, ["CLAUDE_SECURESTORAGE_CONFIG_DIR"]);
    }

    #[test]
    fn a_profiled_codex_launch_names_its_home() {
        let inv = render(
            harness_spec(Harness::Codex),
            Shape::Headless,
            &row_fields(Auth::Inherited, Some("/profiles/codex/work")),
        )
        .unwrap();
        assert_eq!(values(&inv.env, "CODEX_HOME"), ["/profiles/codex/work"]);
        assert!(inv.env_remove.is_empty());
    }

    /// The claude row reads its measured `rate_limit_event` (`tests/fixtures/s1/stdout.jsonl`)
    /// and the fields a refused window adds; nothing else is a reading.
    #[test]
    fn the_claude_stream_reads_its_usage_window() {
        use crate::grammar::{LimitReading, rate_limit};
        let rule = crate::claude_code::STREAM
            .rate_limit
            .as_ref()
            .expect("claude states one");
        let measured = serde_json::json!({"type": "rate_limit_event",
            "rate_limit_info": {"status": "allowed", "isUsingOverage": false}});
        assert_eq!(
            rate_limit(rule, &measured),
            Some(LimitReading {
                status: "allowed".into(),
                resets_at: None,
                window: None
            })
        );
        let refused = serde_json::json!({"type": "rate_limit_event", "rate_limit_info":
            {"status": "rejected", "resetsAt": 1790538000u64, "rateLimitType": "five_hour"}});
        assert_eq!(
            rate_limit(rule, &refused),
            Some(LimitReading {
                status: "rejected".into(),
                resets_at: Some(1_790_538_000),
                window: Some("five_hour".into())
            })
        );
        let other = serde_json::json!({"type": "assistant", "message": {}});
        assert_eq!(rate_limit(rule, &other), None);
    }
}
