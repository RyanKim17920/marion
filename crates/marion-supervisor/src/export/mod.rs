//! **`marion export`**: a report of one delegation tree — what each node was asked, what it did,
//! what it landed and what it spent — written to be shared, as Markdown or as one self-contained
//! HTML file.
//!
//! Read entirely offline: the journal, each node's contract and its `events.jsonl`, exactly the
//! files a supervisor writes. Nothing here dials, starts or waits for a supervisor, so a report can
//! be written for a tree whose supervisor has long gone, on a machine with no harness installed.
//!
//! Every string of a report passes through [`scrub::Scrubber`] once, at one point, before it is
//! rendered, and the rendered text passes through it again.

pub mod scrub;
