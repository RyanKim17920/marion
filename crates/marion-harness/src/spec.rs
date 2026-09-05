//! The declarative harness spec: **one row of data per harness, one renderer for all of them.**
//!
//! Every harness marion drives is launched the same way — a program, an argv, an environment —
//! and until this module existed each adapter wrote that launch out by hand, so 82% of the adapter
//! seam was per-harness code that differed in *values* and not in *shape*. A sixth harness cost
//! four hundred lines of it. This module turns the shape into a small closed vocabulary ([`Arg`],
//! [`Env`]) and the values into a table ([`HarnessSpec`]), so that a harness is a **row**, and the
//! only code a row needs is the handful of measured decisions no table can hold — a refusal, a
//! model rule, a derived URL — which stay in that adapter's `fields` hook and feed the renderer
//! through [`Fields`].
//!
//! # What is data and what is code, and why the line is where it is
//!
//! *Data*: which flags exist, in what order, joined how, gated on what. Those are facts about a
//! binary's `--help`, transcribed once from a measurement and never computed.
//!
//! *Code*: whether a launch is **allowed** — claude refuses an argv prompt because 2.1.220 takes
//! turn one before its MCP server is up; gemini refuses a missing model because `auto` hangs; a
//! canned copilot refuses a missing base URL because the CLI goes looking for a GitHub login. Those
//! are measured *decisions*, each with a fixture behind it, and enum-izing them would turn a
//! sentence of evidence into a variant nobody can check. They live in the adapters.
//!
//! # The risk a table invites
//!
//! A row is easy to write and easy to guess. Two rules keep it honest: `note` is **mandatory** and
//! names the spike that measured the row, and nothing here defaults — an absent value is rendered
//! as absent, never as a plausible stand-in. `adapter::tests::spec_render_matches_the_adapter_that_
//! measured_it` is the invariant: every row renders byte-for-byte what the hand-written adapter it
//! replaced compiled.

use std::path::PathBuf;

use marion_core::harness::Harness;

use crate::auth::Auth;
use crate::grammar::StreamGrammar;
use crate::invocation::Invocation;

/// One harness, as a row: what its launch looks like, stated as data.
#[derive(Debug)]
pub struct HarnessSpec {
    pub harness: Harness,
    /// The binary, or `None` where the launch itself names it — ACP, whose program is the agent's
    /// own and arrives in [`Fields::program`].
    pub program: Option<&'static str>,
    /// The headless argv, in order.
    pub argv: &'static [Arg],
    /// The argv of the pane shape, where the harness has one (§3.4's `opaque`). `None` is a
    /// refusal by name at render time, never a fallback to [`Self::argv`].
    pub pane: Option<&'static [Arg]>,
    /// The environment, in order. Rows gated on [`When`] are the whole of live mode: *"live is a
    /// removal"* is a property of the table, not of five hand-written branches.
    pub env: &'static [Env],
    /// How this harness's output stream is read (§6.1 step 9), or `None` where the reader is code
    /// — ACP, whose stream shape is per agent ([`crate::grammar`]'s module docs).
    pub stream: Option<&'static StreamGrammar>,
    /// **Mandatory.** The spike that measured this row, so a reader can tell a transcription from
    /// a guess. The spec sweep refuses an empty one.
    pub note: &'static str,
}

/// The neutral values a row's [`Arg`]s and [`Env`]s read from.
///
/// Named rather than a map so a row cannot reference a value nobody computes: every variant here
/// is a field of [`Fields`], and the renderer resolves it in exactly one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Cwd,
    ConfigDir,
    /// The model in this harness's own spelling, **after** the adapter's rule — codex compiles
    /// none under a canned provider, opencode wants `provider/model`, gemini refuses `None`.
    Model,
    /// The compiled prompt. Carries a value even when empty; see [`Arg::PosIfNonEmpty`].
    Prompt,
    /// §3.1's **availability** axis in this harness's spelling ([`Axes::tools`]).
    Tools,
    /// §3.1's **permission** axis in this harness's spelling ([`Axes::allowed`]).
    Allowed,
    /// A coarse mode where the harness has one instead of a list ([`Axes::mode`]).
    Mode,
    /// How argv names the MCP declaration document — a path, or copilot's `@path`.
    McpConfig,
    /// The provider base URL in this harness's spelling, or `None` where none is overlaid.
    BaseUrl,
    ApiKey,
    /// Repeatable `key=value` overrides (codex `-c`).
    Pairs,
    /// An inline configuration document carried by an env var (opencode).
    InlineConfig,
    /// A session title (opencode `--title`).
    Title,
    OutputSchema,
    OutputLastMessage,
    /// The argv tail an ACP agent's own table supplies, verbatim.
    AgentArgs,
}

/// One argv element, or the rule for several.
///
/// Closed on purpose: these nine shapes cover five harnesses' `--help` output, and a sixth that
/// needs a ninth should add it here — visibly — rather than write a branch in an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// A literal token.
    Lit(&'static str),
    /// `<flag> <value>` when the field carries a value; nothing otherwise.
    Flag(&'static str, Field),
    /// `<flag>=<value>` when the field carries a value; nothing otherwise.
    FlagEq(&'static str, Field),
    /// `<flag> <item>` once per item of a list field.
    Each(&'static str, Field),
    /// `<flag>=<item>` once per item of a list field.
    EachEq(&'static str, Field),
    /// `<flag> <a,b,…>` — the list comma-joined, emitted **even when empty**. Claude Code's
    /// `--tools ""` is the documented "no built-in tools" spelling, and omitting the flag would be
    /// a different launch.
    Joined(&'static str, Field),
    /// The bare value, when the field carries one.
    Pos(Field),
    /// The bare value, only when it is non-empty. The two TUI shapes: an empty prompt is a pane
    /// opened at its composer, not a positional `""`.
    PosIfNonEmpty(Field),
    /// Every item of a list field, bare.
    Items(Field),
}

/// Where an environment value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val {
    Lit(&'static str),
    /// The field's value; the variable is **omitted** when the field carries none. Present or
    /// absent, never empty — `mcp_bridge::BASE_URL_ENV`'s rule, made structural.
    Field(Field),
    /// A path under the node's config dir: `""` is the dir itself, `"config"` a child.
    Under(&'static str),
}

/// When an [`Env`] row applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// Only under [`Auth::Canned`] — the isolation and the provider overlay, which is what live
    /// mode removes (§6.4).
    Canned,
    /// Only when the field carries a value. Claude Code blanks `ANTHROPIC_API_KEY` **beside** a
    /// token, and only beside one.
    Present(Field),
}

/// One environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    pub key: &'static str,
    pub val: Val,
    pub when: When,
}

/// Which of the row's two argv shapes to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Headless,
    Pane,
}

/// §3.1's two axes and the coarse mode, in **this harness's** spelling.
///
/// Separate from [`Fields`] because the audit record (`compiled_permissions`) needs them without
/// a [`crate::SpawnCtx`], and the adapter's `axes` hook is where an unmappable tool is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Axes {
    pub tools: Vec<String>,
    pub allowed: Vec<String>,
    pub mode: Option<String>,
}

/// Everything a row reads. Built by the adapter's `fields` hook — seeded neutrally from the
/// launch, then adjusted by whatever that harness has measured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    pub cwd: PathBuf,
    pub config_dir: PathBuf,
    pub auth: Auth,
    /// The program, where the row's is `None`.
    pub program: Option<String>,
    pub prompt: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub axes: Axes,
    pub mcp_config: Option<String>,
    pub pairs: Vec<(String, String)>,
    pub inline_config: Option<String>,
    pub title: Option<String>,
    pub output_schema: Option<PathBuf>,
    pub output_last_message: Option<PathBuf>,
    pub agent_args: Vec<String>,
    /// Environment the hook derived that no row names — appended after the row's own. ACP's
    /// per-agent canned recipe is the one user.
    pub extra_env: Vec<(String, String)>,
}

impl Fields {
    /// The scalar a field carries, or the list it carries comma-joined — `None` where it carries
    /// nothing, which for a list means empty.
    fn value(&self, field: Field) -> Option<String> {
        let path = |p: &PathBuf| Some(p.to_string_lossy().into_owned());
        match field {
            Field::Cwd => path(&self.cwd),
            Field::ConfigDir => path(&self.config_dir),
            Field::Model => self.model.clone(),
            Field::Prompt => Some(self.prompt.clone()),
            Field::Mode => self.axes.mode.clone(),
            Field::McpConfig => self.mcp_config.clone(),
            Field::BaseUrl => self.base_url.clone(),
            Field::ApiKey => self.api_key.clone(),
            Field::InlineConfig => self.inline_config.clone(),
            Field::Title => self.title.clone(),
            Field::OutputSchema => self.output_schema.as_ref().and_then(path),
            Field::OutputLastMessage => self.output_last_message.as_ref().and_then(path),
            Field::Tools | Field::Allowed | Field::Pairs | Field::AgentArgs => {
                let items = self.items(field);
                (!items.is_empty()).then(|| items.join(","))
            }
        }
    }

    /// The items of a list field; a scalar is a list of at most one.
    fn items(&self, field: Field) -> Vec<String> {
        match field {
            Field::Tools => self.axes.tools.clone(),
            Field::Allowed => self.axes.allowed.clone(),
            Field::Pairs => self.pairs.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            Field::AgentArgs => self.agent_args.clone(),
            scalar => self.value(scalar).into_iter().collect(),
        }
    }
}

/// The environment `rows` describe, read from `f`, in row order.
///
/// Public because one harness's isolation is another's recipe: an ACP agent that is the opencode
/// binary one subcommand over is pointed at a canned provider by exactly opencode's rows.
pub fn render_env(rows: &[Env], f: &Fields) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for row in rows {
        let applies = match row.when {
            When::Always => true,
            When::Canned => f.auth == Auth::Canned,
            When::Present(field) => f.value(field).is_some(),
        };
        if !applies {
            continue;
        }
        let value = match row.val {
            Val::Lit(s) => Some(s.to_string()),
            Val::Field(field) => f.value(field),
            Val::Under(rel) => Some(
                if rel.is_empty() {
                    f.config_dir.clone()
                } else {
                    f.config_dir.join(rel)
                }
                .to_string_lossy()
                .into_owned(),
            ),
        };
        if let Some(v) = value {
            env.push((row.key.to_string(), v));
        }
    }
    env
}

/// argv + env for one shape of one row, or `None` where the row has no such shape.
///
/// `None` is the pane refusal's raw material: the adapter names the harness in the error, because
/// a pane request answered with the headless launch is the silent downgrade
/// `HarnessError::NoPaneSurface` exists to refuse.
pub fn render(spec: &HarnessSpec, shape: Shape, f: &Fields) -> Option<Invocation> {
    let argv = match shape {
        Shape::Headless => spec.argv,
        Shape::Pane => spec.pane?,
    };
    let program = match spec.program {
        Some(p) => p.to_string(),
        None => f.program.clone()?,
    };
    let mut args: Vec<String> = Vec::new();
    for arg in argv {
        match *arg {
            Arg::Lit(s) => args.push(s.into()),
            Arg::Flag(flag, field) => {
                if let Some(v) = f.value(field) {
                    args.push(flag.into());
                    args.push(v);
                }
            }
            Arg::FlagEq(flag, field) => {
                if let Some(v) = f.value(field) {
                    args.push(format!("{flag}={v}"));
                }
            }
            Arg::Each(flag, field) => {
                for item in f.items(field) {
                    args.push(flag.into());
                    args.push(item);
                }
            }
            Arg::EachEq(flag, field) => {
                for item in f.items(field) {
                    args.push(format!("{flag}={item}"));
                }
            }
            Arg::Joined(flag, field) => {
                args.push(flag.into());
                args.push(f.items(field).join(","));
            }
            Arg::Pos(field) => args.extend(f.value(field)),
            Arg::PosIfNonEmpty(field) => args.extend(f.value(field).filter(|v| !v.is_empty())),
            Arg::Items(field) => args.extend(f.items(field)),
        }
    }
    let mut env = render_env(spec.env, f);
    env.extend(f.extra_env.iter().cloned());
    Some(Invocation {
        program,
        args,
        env,
        cwd: f.cwd.clone(),
        // What the row carried, including its absence: the model that reached argv is the one the
        // hook placed in `Fields::model`, and nothing else is recorded.
        model: f.model.clone(),
    })
}
