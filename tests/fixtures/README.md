# Fixtures

Each `sN/` directory is one measurement: a recording of a real harness CLI (its terminal output,
protocol stream, or the request it sent to a model provider), captured so marion's readers can be
tested against what the vendor's tool actually emits. The per-directory `README.md` says what was
run and what each file shows; `REVIEW.md` is the redaction checklist every fixture passes before it
is committed.

The captured output is included for interoperability testing only. It remains © its respective
owners (Anthropic, GitHub, OpenAI, Google, Block, Alibaba, the opencode authors, and others).
Proprietary system-prompt and tool-description text from closed-source CLIs (Claude Code, GitHub
Copilot CLI) is elided and replaced with `<elided>` or a `<... trimmed>` marker; tool names,
schemas and permission shapes, which the tests read, are kept. Output from open-source harnesses
(opencode, codex, qwen-code, gemini-cli, goose) is kept as recorded.
