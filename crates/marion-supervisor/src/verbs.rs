//! **marion's commands, as a table the library can read**: each command's word, the line `marion
//! --help` lists it under, whether it acts on one agent, and whether it waits for a `y`.
//!
//! The binary's dispatch table (`bin/marion/cli.rs`, `VERBS`) runs them and takes each row's
//! summary from here; a test there holds the two to the same words in the same order. The home
//! screen's `:` line completes and runs commands from this table, so a new command is reachable
//! from the screen the moment it is added.

/// One command of `marion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub name: &'static str,
    /// The line `marion --help` shows; `None` keeps an old spelling out of the list (and off `:`).
    pub summary: Option<&'static str>,
    /// Its first word is an agent `<id>`: the `:` line puts the selected agent's there.
    pub on_agent: bool,
    /// Whether the home screen's `:` line waits for a `y` before running it — as the key for the
    /// same action does, so one action asks one question however it is reached. The command
    /// itself does not ask.
    pub asks: Asks,
}

/// Which of a command's lines end or discard something, or widen what loads on the operator's
/// login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asks {
    Never,
    Always,
    /// Only these subcommands, each by every spelling the command takes: the word after the
    /// command's own, from [`sub`].
    Sub(&'static [&'static str]),
}

impl Spec {
    /// Whether `words` (the command's own word first) wait for a `y`.
    pub fn asks_first(&self, words: &[String]) -> bool {
        match self.asks {
            Asks::Never => false,
            Asks::Always => true,
            Asks::Sub(subs) => words.get(1).is_some_and(|w| subs.contains(&w.as_str())),
        }
    }

    const fn asking(self, asks: Asks) -> Spec {
        Spec { asks, ..self }
    }
}

/// **The subcommand words a `:` line asks first for**, which the commands' own parsers match on
/// too: each spelling is written once and read in both places, so a new one is added here and
/// cannot run from the `:` line without its `y`.
pub mod sub {
    /// `marion key rm` and `marion profile rm`.
    pub const RM: &str = "rm";
    /// The other spelling `marion key` and `marion profile` take for `rm`.
    pub const REMOVE: &str = "remove";
    /// `marion trust allow`.
    pub const ALLOW: &str = "allow";
    /// `marion workflow cancel`.
    pub const CANCEL: &str = "cancel";
}

const fn verb(name: &'static str, summary: &'static str, on_agent: bool) -> Spec {
    Spec {
        name,
        summary: Some(summary),
        on_agent,
        asks: Asks::Never,
    }
}

const fn hidden(name: &'static str) -> Spec {
    Spec {
        name,
        summary: None,
        on_agent: false,
        asks: Asks::Never,
    }
}

/// Every command, in the order `marion --help` lists them.
pub const SPECS: &[Spec] = &[
    verb("run", "start an agent on a task and watch it", false),
    verb("ls", "show the agent tree, or one agent's detail", true),
    verb(
        "search",
        "find past and running agents by what they were asked, said or changed",
        false,
    ),
    verb(
        "race",
        "run one task on several agents and keep the one that passes",
        false,
    ),
    verb(
        "workflow",
        "list, check, run or cancel the workflows this project defines",
        false,
    )
    .asking(Asks::Sub(&[sub::CANCEL])),
    verb(
        "harness",
        "list or check the harness rows your own files define",
        false,
    ),
    verb("attach", "open an agent's terminal", true),
    verb("diff", "show what an agent's branch changed", true),
    verb("merge", "merge an agent's branch into your checkout", true).asking(Asks::Always),
    verb("shell", "open a shell where an agent worked", true),
    verb("steer", "send a running agent a message", true),
    verb("cancel", "stop a running agent", true).asking(Asks::Always),
    verb(
        "resume",
        "bring back an ended agent from its recorded session",
        true,
    ),
    verb("mcp", "serve marion's tools to an MCP client", false),
    verb(
        "key",
        "add, list or remove API keys for model providers",
        false,
    )
    .asking(Asks::Sub(&[sub::RM, sub::REMOVE])),
    // The old spellings of `key add` and `key rm`, listed nowhere; scripts and docs still use them.
    hidden("login"),
    hidden("logout").asking(Asks::Always),
    verb(
        "profile",
        "add, list or choose your logins for a harness",
        false,
    )
    .asking(Asks::Sub(&[sub::RM, sub::REMOVE])),
    verb(
        "export",
        "write a shareable report of an agent and everything under it",
        true,
    ),
    verb(
        "trust",
        "allow the commands a repository's agent types run",
        false,
    )
    .asking(Asks::Sub(&[sub::ALLOW])),
    verb(
        "notify",
        "turn desktop notices of an agent's end on or off",
        false,
    ),
    verb("doctor", "check each harness is installed and ready", false),
];

/// The command `name` names, by its word.
pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}
