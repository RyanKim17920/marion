#!/usr/bin/env python3
"""Fail when a committed capture carries the operator's own inventory or identity.

marion's fixtures are recordings of real harnesses, and a harness started on the operator's own
profile prints what that profile holds: its skills, plugins and MCP servers, its home path, an
email. The repository is public, so none of that may be committed. This scan runs in the security
axis (scripts/three-axis.sh) and after every conformance capture (scripts/conformance.sh).

Three kinds of rule:

- generic, for every runner: a home path (`/Users/<name>`, `/home/<name>`, Claude's
  `-Users-<name>-` project spelling) whose name is not a placeholder, and an email address outside
  the reserved example domains;
- the runner's own identity: its `$HOME` and its user name in a home path;
- the runner's own inventory, read from the machine running the scan and never committed: the
  names of its installed skills, plugins and MCP servers, from the harness config directories
  listed in INVENTORY. A name matches only as a whole token, and names short or common enough to
  collide with ordinary text are skipped (see `distinctive`).

A per-operator needle list, in the demo tooling's redact.tsv shape (`needle<TAB>replacement`, `#`
comments), is read from $MARION_FIXTURE_DENY, else $XDG_CONFIG_HOME/marion/fixture-deny.tsv. It is
local by design: a committed list of the operator's names would be the leak it guards against.

    scripts/fixture-privacy.py [PATH ...]          scan (default: git-tracked tests/fixtures)
    scripts/fixture-privacy.py --fix TSV [PATH ...]  apply TSV's replacements, then scan

`--fix` refuses a replacement whose byte length differs from its needle: the terminal captures are
replayed at recorded byte offsets, so a scrub must not move a single byte.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

# Placeholder names the fixtures use for a home directory on purpose.
HOME_PLACEHOLDERS = {
    "<USER>", "<name>", "<U", "...", "example", "name", "user", "u", "r", "runner",
    "a-rather-long-account-name", "projects", "memories", ".local", ".config",
}
HOME_PATH = re.compile(rb"(?:/Users/|/home/|-Users-)([A-Za-z0-9._<>-]+?)(?=[/\-\"'\s\\]|$)")

EMAIL = re.compile(rb"[A-Za-z0-9._%+-]+@((?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})(?![A-Za-z0-9.-])")
EMAIL_ALLOWED = re.compile(
    rb"(?:^|\.)(?:invalid|test|example|localhost)$|^example\.(?:com|org|net)$|^axo\.dev$"
)

# Where each harness keeps what an operator installs. Directory entries are skill or agent names;
# JSON/TOML files are read for plugin and MCP server names.
INVENTORY_DIRS = [
    ".claude/skills", ".claude/agents", ".codex/skills", ".agents/skills", ".qwen/skills",
    ".gemini/extensions", ".config/opencode/skills", ".config/opencode/agent", ".pi/agent/skills",
]

# Names that are also ordinary words or harness built-ins, so a match proves nothing.
COMMON = {
    "review", "simplify", "handoff", "incident", "synced", "security-review", "update-config",
    "keybindings-help", "skill-creator", "frontend-design", "code-review", "general-purpose",
    "statusline-setup", "claude-code-guide", "deep-research", "design-sync", "codex-cli",
    "browser", "context-management",
}


def distinctive(name: str) -> bool:
    return len(name) >= 6 and name.lower() not in COMMON and not name.startswith(".")


def inventory(home: Path) -> set[str]:
    names: set[str] = set()
    for rel in INVENTORY_DIRS:
        d = home / rel
        if d.is_dir():
            names.update(p.name.removesuffix(".md") for p in d.iterdir())
    plugins = home / ".claude/plugins/installed_plugins.json"
    try:
        data = json.loads(plugins.read_text())
        for key in data.get("plugins", data):
            names.add(key.split("@", 1)[0])
    except (OSError, ValueError, AttributeError):
        pass
    cache = home / ".claude/plugins/cache"
    if cache.is_dir():
        names.update(p.stem for p in cache.glob("*/*/*/agents/*.md"))
    try:
        data = json.loads((home / ".claude.json").read_text())
        names.update(data.get("mcpServers", {}))
        for project in data.get("projects", {}).values():
            names.update(project.get("mcpServers", {}))
    except (OSError, ValueError, AttributeError):
        pass
    try:
        text = (home / ".codex/config.toml").read_text()
        names.update(re.findall(r"^\[mcp_servers\.([A-Za-z0-9_-]+)", text, re.M))
    except OSError:
        pass
    return {n for n in names if distinctive(n)}


def deny_file() -> Path | None:
    env = os.environ.get("MARION_FIXTURE_DENY")
    if env:
        return Path(env)
    base = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    p = base / "marion/fixture-deny.tsv"
    return p if p.is_file() else None


def read_tsv(path: Path) -> list[tuple[bytes, bytes | None]]:
    rows = []
    for line in path.read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        needle, _, repl = line.partition("\t")
        rows.append((needle.encode(), repl.encode() if repl else None))
    return rows


def token(needle: bytes) -> re.Pattern:
    return re.compile(rb"(?<![A-Za-z0-9_-])" + re.escape(needle) + rb"(?![A-Za-z0-9_-])")


def targets(args: list[str]) -> list[Path]:
    if args:
        out = []
        for a in args:
            p = Path(a)
            out.extend(sorted(q for q in p.rglob("*") if q.is_file()) if p.is_dir() else [p])
        return out
    listed = subprocess.run(
        ["git", "ls-files", "-z", "tests/fixtures"], capture_output=True, check=True
    ).stdout
    return [Path(p) for p in listed.decode().split("\0") if p]


def fix(tsv: Path, files: list[Path]) -> None:
    rows = read_tsv(tsv)
    for needle, repl in rows:
        if repl is None or len(repl) != len(needle):
            sys.exit(f"fixture-privacy: `{needle.decode()}` needs a same-length replacement")
    for f in files:
        data = f.read_bytes()
        new = data
        for needle, repl in rows:
            new = token(needle).sub(repl, new)
        if new != data:
            f.write_bytes(new)
            print(f"fixture-privacy: scrubbed {f}", file=sys.stderr)


def main() -> int:
    args = sys.argv[1:]
    fix_tsv = None
    if args[:1] == ["--fix"]:
        if len(args) < 2:
            sys.exit(__doc__)
        fix_tsv, args = Path(args[1]), args[2:]
    files = targets(args)
    if fix_tsv:
        fix(fix_tsv, files)

    home = Path.home()
    user = os.environ.get("USER") or home.name
    needles = {n.encode(): "installed on this machine" for n in inventory(home)}
    deny = deny_file()
    if deny:
        needles.update({n: f"listed in {deny}" for n, _ in read_tsv(deny)})
    patterns = [(token(n), n, why) for n, why in sorted(needles.items())]
    own_home = str(home).encode() if len(str(home)) > 5 else None

    findings = []
    for f in files:
        try:
            data = f.read_bytes()
        except OSError:
            continue
        for m in HOME_PATH.finditer(data):
            name = m.group(1).decode(errors="replace")
            if name not in HOME_PLACEHOLDERS or name == user:
                findings.append((f, m.start(), f"home path `{m.group(0).decode(errors='replace')}`"))
        for m in EMAIL.finditer(data):
            if not EMAIL_ALLOWED.search(m.group(1).lower()):
                findings.append((f, m.start(), f"email `{m.group(0).decode(errors='replace')}`"))
        if own_home and own_home in data:
            findings.append((f, data.index(own_home), "this machine's $HOME"))
        for pat, needle, why in patterns:
            m = pat.search(data)
            if m:
                findings.append((f, m.start(), f"`{needle.decode()}` ({why})"))

    for f, offset, what in findings:
        line = f.read_bytes()[:offset].count(b"\n") + 1
        print(f"{f}:{line}: {what}")
    if findings:
        print(
            f"\nfixture-privacy: {len(findings)} finding(s). Replace each with a neutral placeholder"
            " of the same byte length (scripts/fixture-privacy.py --fix <tsv>), and record it in"
            " tests/fixtures/REVIEW.md.",
            file=sys.stderr,
        )
        return 1
    print(f"fixture-privacy: ok ({len(files)} files, {len(patterns)} local names)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
