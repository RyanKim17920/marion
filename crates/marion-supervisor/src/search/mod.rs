//! **Search across runs**: which run did X, in this project or in every project under `<state>`.
//!
//! Every project keeps one small append-only digest, `<state>/private/<hash>/runs.jsonl`, with one
//! line per run that ended: its metadata, its task, its report, its checks, its findings and the
//! paths it changed ([`digest`]). A search scans that file newest-first. No index, no daemon and
//! nothing resident between searches: idle costs nothing.
//!
//! The digest is a **projection**, never a source of truth. Everything in it is already in the
//! project's journal, contracts, `prompt.txt` and `root-change.json`, masked by the scrubber every
//! report passes through, so it widens nobody's access. It lives under `<state>/private/`, `0700`
//! with `0600` files, which the OS sandbox denies a node outright once it ships.
//!
//! What search never reads: a pane's `pty.cast` (its output echoes what the operator typed), and
//! steer or send text (the journal keeps only its length and digest).

pub mod digest;
pub mod open;
