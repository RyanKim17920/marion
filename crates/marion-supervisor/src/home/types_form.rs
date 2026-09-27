//! **Setup's agent-type form**: the one custom type the operator is writing, and what adding it
//! does to `.marion/agents.toml`.
//!
//! Pure on purpose, like the rest of the state machine: [`apply`] turns the file's text and a
//! [`Draft`] into the new text, and [`diff`] says what changed. The session reads the file, shows
//! the diff, and writes only after a `y` — and only text the spawn path's own loader
//! ([`crate::run::agent_types_text`]) accepts, so the form can never leave a file that refuses every
//! spawn.
//!
//! The file is edited with `toml_edit`, not re-serialised: the operator's comments, ordering and
//! spacing survive, and a type already in the file is updated in place rather than duplicated.

use marion_core::agent_type::{TOOL_READ, TOOL_WRITE};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

/// What the type may do, as the file's `tools` key spells it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tools {
    /// No `tools` key: the harness's own default grant.
    #[default]
    Default,
    /// `["read"]`: a read-only type that delegates.
    Read,
    /// `["read", "write"]`: an implementer.
    ReadWrite,
}

impl Tools {
    pub const ALL: [Tools; 3] = [Tools::Default, Tools::Read, Tools::ReadWrite];

    /// How the form shows the choice.
    pub fn word(self) -> &'static str {
        match self {
            Tools::Default => "harness default",
            Tools::Read => TOOL_READ,
            Tools::ReadWrite => "read, write",
        }
    }

    fn list(self) -> Option<Vec<&'static str>> {
        match self {
            Tools::Default => None,
            Tools::Read => Some(vec![TOOL_READ]),
            Tools::ReadWrite => Some(vec![TOOL_READ, TOOL_WRITE]),
        }
    }
}

/// One custom agent type as the form holds it. Empty optional text is "not stated".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub name: String,
    /// As the file spells it (`codex`, `claude`).
    pub harness: String,
    pub model: String,
    pub description: String,
    pub tools: Tools,
    pub provider: String,
}

/// `text` with `d` written into it: the `[[agent]]` row named `d.name` updated in place when there
/// is one, else a new row at the end. A key the draft leaves empty is removed from an existing row,
/// so the file says what the form showed. `Err` when `text` is not TOML or `agent` is not an array
/// of tables — the loader's refusal would say the same, and the file is left alone.
pub fn apply(text: &str, d: &Draft) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("not TOML: {e}"))?;
    let agents = doc
        .entry("agent")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or("`agent` is not a list of [[agent]] rows")?;
    let existing = agents
        .iter_mut()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(d.name.as_str()));
    match existing {
        Some(row) => fill(row, d),
        None => {
            let mut row = Table::new();
            fill(&mut row, d);
            agents.push(row);
        }
    }
    Ok(doc.to_string())
}

fn fill(row: &mut Table, d: &Draft) {
    let mut set = |key: &str, v: &str| {
        if v.trim().is_empty() {
            row.remove(key);
        } else {
            row[key] = value(v.trim());
        }
    };
    set("name", &d.name);
    set("harness", &d.harness);
    set("description", &d.description);
    set("model", &d.model);
    set("provider", &d.provider);
    match d.tools.list() {
        None => {
            row.remove("tools");
        }
        Some(tools) => {
            row["tools"] = value(tools.into_iter().collect::<Array>());
        }
    }
}

/// One line of a diff: `+` added, `-` removed, ` ` kept for context.
pub type DiffLine = (char, String);

/// The lines that changed from `old` to `new`, each with up to [`CONTEXT`] unchanged lines around
/// it; runs of unchanged lines further away are left out. Empty when nothing changed.
pub fn diff(old: &str, new: &str) -> Vec<DiffLine> {
    use similar::{ChangeTag, TextDiff};
    let d = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for group in d.grouped_ops(CONTEXT) {
        for op in group {
            for c in d.iter_changes(&op) {
                let tag = match c.tag() {
                    ChangeTag::Insert => '+',
                    ChangeTag::Delete => '-',
                    ChangeTag::Equal => ' ',
                };
                out.push((tag, c.value().trim_end_matches('\n').to_string()));
            }
        }
    }
    out
}

/// Unchanged lines shown around a change.
pub const CONTEXT: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str) -> Draft {
        Draft {
            name: name.into(),
            harness: "codex".into(),
            model: String::new(),
            description: "Reviews diffs.".into(),
            tools: Tools::Read,
            provider: String::new(),
        }
    }

    #[test]
    fn a_new_type_is_appended_and_the_operators_comments_survive() {
        let old = "# our agents\n\n[[agent]]\nname = \"migrator\" # keep me\nharness = \"claude\"\ndescription = \"Moves data.\"\n";
        let new = apply(old, &draft("reviewer")).unwrap();
        assert!(new.starts_with(old), "the old text is untouched:\n{new}");
        assert!(new.contains("# keep me"));
        let types = marion_core::agent_type::AgentTypes::parse(&new).expect("the loader takes it");
        let r = types.resolve("reviewer").expect("the new row");
        assert_eq!(r.harness, marion_core::harness::Harness::Codex);
        assert_eq!(
            r.tools,
            vec![TOOL_READ.to_string()],
            "the tools the form chose"
        );
    }

    #[test]
    fn a_type_already_in_the_file_is_updated_in_place_and_cleared_keys_go() {
        let old = "[[agent]]\nname = \"reviewer\"\nharness = \"claude\"\ndescription = \"Old.\"\nmodel = \"opus\"\n";
        let mut d = draft("reviewer");
        d.tools = Tools::ReadWrite;
        let new = apply(old, &d).unwrap();
        assert_eq!(new.matches("[[agent]]").count(), 1, "{new}");
        assert!(!new.contains("model"), "an emptied field is removed: {new}");
        assert!(new.contains("harness = \"codex\""));
        assert!(new.contains("tools = [\"read\", \"write\"]"), "{new}");
    }

    #[test]
    fn a_missing_or_empty_file_gets_its_first_row() {
        let new = apply("", &draft("reviewer")).unwrap();
        assert!(new.contains("[[agent]]\nname = \"reviewer\""), "{new}");
        marion_core::agent_type::AgentTypes::parse(&new).expect("the loader takes it");
    }

    #[test]
    fn a_file_that_is_not_toml_is_refused_rather_than_rewritten() {
        assert!(apply("[[agent]\nname =", &draft("x")).is_err());
        assert!(apply("agent = 3\n", &draft("x")).is_err());
    }

    #[test]
    fn the_diff_shows_the_change_and_a_little_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let d = diff(old, new);
        assert_eq!(
            d,
            vec![
                (' ', "f".to_string()),
                (' ', "g".to_string()),
                ('+', "h".to_string())
            ]
        );
        assert!(diff(old, old).is_empty());
    }
}
