//! **`marion`'s command table, its help, and the one way its commands read their words.**
//!
//! Every command is one row of [`VERBS`]: its word, the old spellings still accepted, the line
//! `marion --help` lists it under, its own `--help`, and the function that runs it. Adding a
//! command is adding a row, and `marion --help` cannot forget it.
//!
//! Each command still parses its own flags — a flag one command takes means nothing to another,
//! and a shared parser is how a flag ends up accepted by a command that ignores it. What they
//! share is [`Words`] (so `--help`, `--` and `--flag=value` mean the same everywhere) and the two
//! flag groups several commands take: [`Place`] (`--repo`, `--state-dir`) and [`Backend`]
//! (`--canned`, `--base-url`). **An unknown flag is always refused**, never ignored: a mistyped
//! `--state-dir` that fell through would quietly talk to a different supervisor.

use std::path::PathBuf;
use std::process::ExitCode;

use marion_core::agent_type::builtin_names;
use marion_core::production_native_facades;

/// One command of `marion`.
pub struct Verb {
    pub name: &'static str,
    /// Old spellings that still run this command and are listed nowhere.
    pub aliases: &'static [&'static str],
    /// The line `marion --help` shows; `None` keeps the command out of the list.
    pub summary: Option<&'static str>,
    pub help: fn() -> String,
    /// Runs the command on the whole argv, the command's own word first.
    pub main: fn(&[String]) -> Result<ExitCode, Exit>,
}

/// Why a command did not run: `--help` was asked for, or its words did not parse.
#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Help,
    Usage(String),
}

/// Every command, in the order `marion --help` lists them.
pub const VERBS: &[Verb] = &[
    Verb {
        name: "run",
        aliases: &[],
        summary: Some("start an agent on a task and watch it"),
        help: run_help,
        main: super::run_main,
    },
    Verb {
        name: "ls",
        aliases: &["tree"],
        summary: Some("show the agent tree, or one agent's detail"),
        help: ls_help,
        main: super::ls_main,
    },
    // `ls`, printed as lines even on a terminal. Kept for scripts and the home screen's hints.
    Verb {
        name: "list",
        aliases: &[],
        summary: None,
        help: ls_help,
        main: super::list_main,
    },
    Verb {
        name: "attach",
        aliases: &[],
        summary: Some("open an agent's terminal"),
        help: attach_help,
        main: super::attach_main,
    },
    Verb {
        name: "steer",
        aliases: &[],
        summary: Some("send a running agent a message"),
        help: steer_help,
        main: super::steer_main,
    },
    Verb {
        name: "cancel",
        aliases: &[],
        summary: Some("stop a running agent"),
        help: cancel_help,
        main: super::cancel_main,
    },
    Verb {
        name: "resume",
        aliases: &[],
        summary: Some("bring back an agent whose supervisor stopped"),
        help: resume_help,
        main: super::resume_main,
    },
    Verb {
        name: "mcp",
        aliases: &[],
        summary: Some("serve marion's tools to an MCP client"),
        help: mcp_help,
        main: super::mcp_main,
    },
    Verb {
        name: "login",
        aliases: &[],
        summary: Some("store an API key for a provider"),
        help: login_help,
        main: super::login_main,
    },
    Verb {
        name: "logout",
        aliases: &[],
        summary: Some("remove a stored API key"),
        help: login_help,
        main: super::login_main,
    },
    Verb {
        name: "profile",
        aliases: &[],
        summary: Some("add, list or choose your logins for a harness"),
        help: profile_help,
        main: super::profile_main,
    },
    Verb {
        name: "trust",
        aliases: &[],
        summary: Some("allow the commands a repository's agent types run"),
        help: trust_help,
        main: super::trust_main,
    },
    Verb {
        name: "doctor",
        aliases: &[],
        summary: Some("check each harness is installed and ready"),
        help: doctor_help,
        main: super::doctor_main,
    },
];

/// The command `word` names, by its name or an old spelling.
pub fn verb(word: &str) -> Option<&'static Verb> {
    VERBS
        .iter()
        .find(|v| v.name == word || v.aliases.contains(&word))
}

/// Run `argv` (the words after `marion`, a command's word first) through its row, and turn an
/// [`Exit`] into what the operator sees: the command's help, or one line saying what was wrong and
/// where the help is.
pub fn dispatch(verb: &Verb, argv: &[String]) -> ExitCode {
    match (verb.main)(argv) {
        Ok(code) => code,
        Err(Exit::Help) => {
            println!("{}", (verb.help)());
            ExitCode::SUCCESS
        }
        Err(Exit::Usage(why)) => {
            eprintln!(
                "marion {}: {why}\n`marion {} --help` shows how to use it",
                verb.name, verb.name
            );
            ExitCode::from(2)
        }
    }
}

/// For a command whose own module parses its words (`login`, `profile`, `doctor`): `--help` or
/// `-h` anywhere before a `--` is a request for help.
pub fn asks_for_help(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--help" || a == "-h")
}

// --- reading words ------------------------------------------------------------------------------

/// One word after a command.
#[derive(Debug, PartialEq, Eq)]
pub enum Word<'a> {
    /// `--name`, with the value of a `--name=value` spelling.
    Flag(&'a str, Option<&'a str>),
    /// Anything else, including a lone `-` (stdin) and every word after `--`.
    Plain(&'a str),
}

/// The words after a command, one at a time. `--help`/`-h` before a `--` is [`Exit::Help`]; after
/// `--`, or once the command calls [`Self::rest`], every word is plain — so `marion steer ab12 --
/// --help` sends the text `--help`.
pub struct Words<'a> {
    it: std::slice::Iter<'a, String>,
    raw: bool,
}

impl<'a> Words<'a> {
    /// The words of `argv` after its first (the command's own word).
    pub fn after_verb(argv: &'a [String]) -> Self {
        Self {
            it: argv.get(1..).unwrap_or_default().iter(),
            raw: false,
        }
    }

    pub fn next(&mut self) -> Result<Option<Word<'a>>, Exit> {
        let Some(word) = self.it.next() else {
            return Ok(None);
        };
        if self.raw {
            return Ok(Some(Word::Plain(word)));
        }
        match word.as_str() {
            "--" => {
                self.raw = true;
                self.next()
            }
            "--help" | "-h" => Err(Exit::Help),
            w if w.starts_with("--") => Ok(Some(match w.split_once('=') {
                Some((flag, value)) => Word::Flag(flag, Some(value)),
                None => Word::Flag(w, None),
            })),
            w if w.starts_with('-') && w.len() > 1 => Ok(Some(Word::Flag(w, None))),
            w => Ok(Some(Word::Plain(w))),
        }
    }

    /// The value of `flag`: its `=value`, else the next word whatever it looks like.
    pub fn value(&mut self, flag: &str, inline: Option<&str>) -> Result<String, Exit> {
        match inline {
            Some(v) => Ok(v.to_string()),
            None => self
                .it
                .next()
                .cloned()
                .ok_or_else(|| Exit::Usage(format!("{flag} needs a value"))),
        }
    }

    /// Every word left, all plain.
    pub fn rest(&mut self) -> Vec<&'a str> {
        self.raw = true;
        self.it.by_ref().map(String::as_str).collect()
    }
}

/// A flag that takes no value, refusing a `--flag=value` spelling.
pub fn switch(flag: &str, inline: Option<&str>) -> Result<bool, Exit> {
    match inline {
        None => Ok(true),
        Some(_) => Err(Exit::Usage(format!("{flag} takes no value"))),
    }
}

pub fn unknown(flag: &str) -> Exit {
    Exit::Usage(format!("unknown option {flag}"))
}

/// The one positional word a command needs, or the refusal naming it.
pub fn required(word: Option<&str>, what: &str) -> Result<String, Exit> {
    word.map(str::to_string)
        .ok_or_else(|| Exit::Usage(format!("needs {what}")))
}

/// **Which project**: `--repo` and `--state-dir`, the pair every command that talks to a supervisor
/// resolves the same way, because the supervisor is keyed on both.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub repo: Option<PathBuf>,
    pub state_dir: Option<String>,
}

impl Place {
    /// `true` when `flag` was one of the pair and has been taken.
    pub fn take(&mut self, flag: &str, inline: Option<&str>, w: &mut Words) -> Result<bool, Exit> {
        match flag {
            "--repo" => self.repo = Some(PathBuf::from(w.value(flag, inline)?)),
            "--state-dir" => self.state_dir = Some(w.value(flag, inline)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// **Which provider**: `--canned` and its `--base-url`, for the commands that may start agents.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Backend {
    /// Opt **in** to marion's canned provider; real auth is what a person at a terminal means.
    pub canned: bool,
    pub base_url: Option<String>,
}

impl Backend {
    /// `true` when `flag` was one of this group and has been taken.
    pub fn take(&mut self, flag: &str, inline: Option<&str>, w: &mut Words) -> Result<bool, Exit> {
        match flag {
            "--canned" => self.canned = switch(flag, inline)?,
            "--base-url" => self.base_url = Some(w.value(flag, inline)?),
            // Accepted and inert. It used to select real auth, which is now the default; scripts
            // and notes that carry it keep working.
            "--live" => {
                switch(flag, inline)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn auth(&self) -> marion_harness::Auth {
        if self.canned {
            marion_harness::Auth::Canned
        } else {
            marion_harness::Auth::Inherited
        }
    }
}

// --- help ---------------------------------------------------------------------------------------

const PLACE_HELP: &str = concat!(
    "  --repo <path>        the repository (default: the enclosing git repository, else here)\n",
    "  --state-dir <path>   where marion keeps its state (default: $MARION_STATE_DIR, else\n",
    "                       $XDG_STATE_HOME/marion, else ~/.local/state/marion)",
);

const BACKEND_HELP: &str = concat!(
    "  --canned             use marion's free test provider instead of your login (start it\n",
    "                       first with `marion-canned`); it answers nothing useful\n",
    "  --base-url <url>     where that test provider listens (only with --canned)",
);

const ID_HELP: &str =
    "<id> is an agent's whole id, the short id its tree row shows, or a unique start of its id.";

/// `marion --help`: what marion is, and one line per command.
pub fn top_help() -> String {
    let native = production_native_facades()
        .enabled_native_commands()
        .join(", ");
    let commands: String = VERBS
        .iter()
        .filter_map(|v| v.summary.map(|s| format!("  {:<9}{s}\n", v.name)))
        .collect();
    format!(
        "marion runs coding agents as subagents of one another, and shows you the tree.\n\
         \n\
         usage: marion                      open the home screen: start, watch and set up agents\n\
         \x20      marion <harness> [<flags>]  run that harness's own TUI with marion connected\n\
         \x20                                  ({native})\n\
         \x20      marion <command> [<args>]\n\
         \n\
         commands:\n\
         {commands}\
         \n\
         {ID_HELP}\n\
         `marion <command> --help` explains a command. `marion --version` prints the version.\n\
         Inside a harness's TUI, ^] d detaches and ^] s toggles a status row."
    )
}

/// `names` as lines no wider than `width`, each indented by `indent`.
fn wrapped(names: &[&str], indent: &str, width: usize) -> String {
    let mut out = String::new();
    let mut line = String::from(indent);
    for name in names {
        if line.len() > indent.len() && line.len() + name.len() + 2 > width {
            out.push_str(line.trim_end());
            out.push('\n');
            line = String::from(indent);
        }
        line.push_str(name);
        line.push_str(", ");
    }
    out.push_str(line.trim_end().trim_end_matches(','));
    out
}

fn run_help() -> String {
    format!(
        "usage: marion run <agent-type> --prompt <text> [options]\n\
         \n\
         Start an agent on a task in this repository and watch it until it ends. What it does\n\
         is shown on stderr; its raw output goes to stdout when stdout is not a terminal.\n\
         \n\
         \x20 --prompt <text>      the task (required)\n\
         \x20 --model <name>       the model; `openrouter:<model>` runs on a key from `marion login`\n\
         \x20 --timeout <secs>     stop the agent after this many seconds (default: its type's)\n\
         \x20 --profile <name>     which of your logins to use (see `marion profile`)\n\
         \x20 --pane               run it in a terminal you open with `marion attach`, and return\n\
         \x20 --detach             run it in the background, and return; `marion ls` watches it\n\
         \x20 --no-change-record   skip the snapshot of your checkout taken when it starts and\n\
         \x20                      ends; the agent then gets no file tools. Needed outside git.\n\
         {BACKEND_HELP}\n\
         {PLACE_HELP}\n\
         \n\
         agent types, plus any in the repository's .marion/agents.toml:\n\
         {types}\n\
         A plain harness name is that harness working on the task; <harness>-orchestrator only\n\
         plans and delegates.\n\
         \n\
         An agent uses the login you already have for its harness and makes real model calls\n\
         that cost real money. marion never logs in to a harness for you.",
        types = wrapped(builtin_names(), "  ", 90),
    )
}

fn ls_help() -> String {
    format!(
        "usage: marion ls [<id>] [--attention] [--repo <path>] [--state-dir <path>]\n\
         \n\
         Show the agent tree. On a terminal it opens the home screen's Watch view; piped, or\n\
         with --attention, it prints one agent per line. With an <id> it prints that agent's\n\
         detail: its task, the messages it was sent, its tokens, where it worked and how it\n\
         ended.\n\
         \n\
         \x20 --attention          only the agents that need you: blocked, failed, timed out,\n\
         \x20                      ended without reporting, or orphaned\n\
         {PLACE_HELP}\n\
         \n\
         {ID_HELP}\n\
         It never starts a supervisor. `marion tree` is its old name; `marion list` prints the\n\
         lines even on a terminal."
    )
}

fn attach_help() -> String {
    format!(
        "usage: marion attach <id> [--repo <path>] [--state-dir <path>]\n\
         \n\
         Open the terminal of an agent started with `marion run --pane`: see its screen and\n\
         type into it. ^] d detaches and leaves it running.\n\
         \n\
         {PLACE_HELP}\n\
         \n\
         {ID_HELP}"
    )
}

fn steer_help() -> String {
    format!(
        "usage: marion steer <id> [--repo <path>] [--state-dir <path>] [--] <message…>\n\
         \x20      marion steer <id> -          read the message from stdin\n\
         \n\
         Send a running agent a message, as you; it reads it at its next turn. Prints which\n\
         agent takes it and exits 0, or exits 1 with the reason it was refused. Options go\n\
         before the message; `--` ends them, so the message may start with a dash.\n\
         \n\
         {PLACE_HELP}\n\
         \n\
         {ID_HELP}\n\
         It never starts a supervisor."
    )
}

fn cancel_help() -> String {
    format!(
        "usage: marion cancel <id> [--repo <path>] [--state-dir <path>]\n\
         \n\
         Stop a running agent. It is recorded as cancelled, and its children keep running. It\n\
         does not ask first: typing the command is the confirmation.\n\
         \n\
         {PLACE_HELP}\n\
         \n\
         {ID_HELP}\n\
         It never starts a supervisor."
    )
}

fn resume_help() -> String {
    format!(
        "usage: marion resume <id> [--prompt <text>] [options]\n\
         \n\
         Bring back a headless agent whose supervisor stopped, from its recorded session, and\n\
         watch it as `marion run` does. --prompt gives it something new to start from.\n\
         \n\
         \x20 --prompt <text>      a message to resume with\n\
         {BACKEND_HELP}\n\
         {PLACE_HELP}\n\
         \n\
         {ID_HELP} It is looked up in this\n\
         project's journal, so it works with no supervisor running."
    )
}

fn mcp_help() -> String {
    let tools = super::mcp_tool_names();
    format!(
        "usage: marion mcp [--repo <path>] [--state-dir <path>] [--canned [--base-url <url>]]\n\
         \n\
         Serve marion's tools ({tools}) over stdio, for an MCP client you configure:\n\
         \x20 command: \"marion\", args: [\"mcp\", \"--repo\", \"/path/to/repo\"]\n\
         Its spawn starts an agent the way `marion run` does. stdout carries only JSON-RPC.\n\
         \n\
         {BACKEND_HELP}\n\
         {PLACE_HELP}",
        tools = tools.join(", "),
    )
}

fn login_help() -> String {
    format!(
        "{}\n\
         \n\
         An agent whose type or --model names a provider (`--model openrouter:<model>`) runs on\n\
         the key stored for it. --list shows each provider and which keys are stored, never a\n\
         key. marion never reuses a vendor's subscription login.",
        marion_supervisor::login::USAGE
    )
}

fn profile_help() -> String {
    format!(
        "{}\n\
         \n\
         A profile is another login for a harness, kept in its own directory. `add` creates it\n\
         and prints the one command that logs in (marion never runs it); `list` shows each\n\
         profile's login state and last usage; `use` makes one the harness's default; `remove`\n\
         forgets one, and --purge also deletes its directory. `marion run --profile <name>`\n\
         picks one for a single run.",
        marion_supervisor::profile_cli::USAGE
    )
}

fn trust_help() -> String {
    format!(
        "{}\n\
         \n\
         A repository's .marion/agents.toml can name a command for an agent type to run. marion\n\
         runs it only after you have read the file and allowed it: `allow` records the file's\n\
         content hash, and any later edit needs allowing again; `deny` forgets it; `list` shows\n\
         what you have allowed.",
        marion_supervisor::trust::USAGE
    )
}

fn doctor_help() -> String {
    format!(
        "{}\n\
         \n\
         Check each harness: whether it is installed, its version, and what marion can do with\n\
         it, plus the API keys `marion login` stored. It makes no model call unless --adapter is\n\
         given, which runs one tiny real task per harness (and costs a little). --harness checks\n\
         one; --acp-command adds an ACP agent by its command line; --providers checks only the\n\
         stored keys.",
        marion_supervisor::doctor::USAGE
    )
}
