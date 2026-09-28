#!/usr/bin/env python3
"""Read one live-smoke scenario's marion state back into a result row and a redacted transcript.

Called by scripts/live-smoke.sh once per scenario, after the root has ended (or been killed at the
scenario's wall clock). It reads only what marion wrote: the project journal (who spawned whom, on
which harness and version, what each run spent, which steers were delivered or dropped), each
child's persisted contract (reported or synthesized, verification evidence, the landed branch), and
each node's events.jsonl (the harness's own frames, which become the transcript).

The verdict on the code is the fixture's real test command plus the scenario's own check, run on the
landed branch in a fresh worktree of the fixture repo — never the child's word for it, and never
marion's own verification evidence, which is recorded beside it.

The transcript is redacted before it is written: home paths, the user name, the scenario's scratch
root, UUIDs, emails and anything shaped like a key or bearer token are replaced, and the harness's
catalogue frames (claude's `system/init`: skills, plugins, MCP servers, memory paths) are reduced
to the model name, because tests/fixtures/REVIEW.md lists those as operator environment.
"""

import argparse
import getpass
import json
import os
import re
import subprocess
import sys
from pathlib import Path

# A frame string longer than this is cut: the transcript is for a human to skim, not a replay.
MAX_STR = 400
# Frames kept per node, first and last halves, so a long run keeps its opening and its end.
MAX_FRAMES = 160

SECRET_PATTERNS = [
    re.compile(r"sk-[A-Za-z0-9_-]{16,}"),
    re.compile(r"Bearer\s+[A-Za-z0-9._~+/=-]{8,}"),
    re.compile(r"gh[pousr]_[A-Za-z0-9]{20,}"),
    re.compile(r"xox[baprs]-[A-Za-z0-9-]+"),
    re.compile(r"AKIA[0-9A-Z]{16}"),
    re.compile(r"AIza[0-9A-Za-z_-]{30,}"),
    re.compile(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
    re.compile(r"-----BEGIN [A-Z ]+-----"),
]
UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
# claude's init frame carries the operator's catalogue; keep only what the transcript needs.
CATALOGUE_KEYS = {
    "agents", "skills", "slash_commands", "plugins", "mcp_servers", "memory_paths", "tools",
    "terminal_slash_commands", "capabilities", "cwd", "session_id", "uuid",
}


class Redactor:
    def __init__(self, scratch: str):
        home = str(Path.home())
        user = getpass.getuser()
        # Longest first, so the scratch root (under $TMPDIR) wins over any shorter prefix.
        self.literals = sorted(
            {
                (scratch, "<SCRATCH>"),
                (os.path.realpath(scratch), "<SCRATCH>"),
                (home, "<HOME>"),
                (home.replace("/", "-"), "-<HOME>"),
                (user, "<USER>"),
            },
            key=lambda p: -len(p[0]),
        )

    def text(self, s: str) -> str:
        for pat in SECRET_PATTERNS:
            s = pat.sub("<REDACTED>", s)
        for lit, rep in self.literals:
            if lit:
                s = s.replace(lit, rep)
        s = UUID.sub("<UUID>", s)
        s = EMAIL.sub("redacted@example.invalid", s)
        return s

    def value(self, v, cut=True):
        if isinstance(v, str):
            s = self.text(v)
            if cut and len(s) > MAX_STR:
                s = s[:MAX_STR] + f"…[{len(s) - MAX_STR} more chars]"
            return s
        if isinstance(v, list):
            return [self.value(x, cut) for x in v]
        if isinstance(v, dict):
            return {self.text(k): self.value(x, cut) for k, x in v.items()}
        return v


def read_jsonl(path: Path):
    if not path.exists():
        return []
    out = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


def kind_of(rec):
    k = rec.get("kind")
    if isinstance(k, dict) and len(k) == 1:
        return next(iter(k.items()))
    return (k, None)


def add_usage(a, b):
    keys = ("input", "output", "cache_read", "cache_write", "reasoning")
    return {k: (a.get(k) or 0) + (b.get(k) or 0) for k in keys}


def project_dir(state: Path) -> Path:
    dirs = [p for p in state.iterdir() if p.is_dir() and (p / "journal.jsonl").exists()]
    if len(dirs) != 1:
        raise SystemExit(f"expected one project dir with a journal under {state}, found {dirs}")
    return dirs[0]


def walk_nodes(pdir: Path):
    """Every node marion journaled, in spawn order, with what the journal says about it."""
    nodes = {}
    order = []
    steers = []
    for rec in read_jsonl(pdir / "journal.jsonl"):
        name, body = kind_of(rec)
        if not isinstance(body, dict):
            continue
        aid = body.get("agent_id")
        if name == "SpawnIntent":
            nodes[aid] = {
                "agent_id": aid,
                "parent_id": body.get("parent_id"),
                "agent_type": body.get("agent_type"),
                "harness": body.get("harness"),
                "depth": body.get("depth"),
                "version": None,
                "generations": 0,
                "usage": None,
                "exit": None,
                "spawned_ts": rec.get("ts"),
                "exited_ts": None,
            }
            order.append(aid)
        elif aid in nodes and name == "Spawned":
            nodes[aid]["version"] = body.get("harness_version")
            nodes[aid]["generations"] += 1
        elif aid in nodes and name == "UsageRecorded":
            u = body.get("usage") or {}
            prev = nodes[aid]["usage"]
            nodes[aid]["usage"] = add_usage(prev, u) if prev else add_usage({}, u)
        elif aid in nodes and name == "Exited":
            nodes[aid]["exit"] = body.get("status")
            nodes[aid]["exited_ts"] = rec.get("ts")
        elif name in ("MessageQueued", "MessageDelivered", "MessageDropped"):
            steers.append({"record": name, "agent_id": aid, "detail": body})
    return [nodes[a] for a in order], steers


def contracts(pdir: Path):
    out = []
    for path in sorted(pdir.glob("agents/*/contracts/*.json")):
        try:
            out.append((path, json.loads(path.read_text())))
        except (OSError, json.JSONDecodeError):
            continue
    return out


def node_cost_usd(events):
    """claude's `result` frames state a list-price cost; other harnesses state none."""
    total = None
    for ev in events:
        vendor = (ev.get("payload") or {}).get("Vendor") if isinstance(ev.get("payload"), dict) else None
        if vendor and vendor.get("key") == "result":
            c = (vendor.get("json") or {}).get("total_cost_usd")
            if isinstance(c, (int, float)):
                total = (total or 0) + c
    return total


def tool_calls(events):
    """Tool names a node called, read generically off its frames (any `name` beside an input)."""
    names = []

    def visit(v):
        if isinstance(v, dict):
            name = v.get("name") or v.get("tool")
            if isinstance(name, str) and any(k in v for k in ("input", "arguments", "args")):
                names.append(name)
            for x in v.values():
                visit(x)
        elif isinstance(v, list):
            for x in v:
                visit(x)

    for ev in events:
        payload = ev.get("payload")
        if isinstance(payload, dict) and "Vendor" in payload:
            visit(payload["Vendor"].get("json"))
    return names


def transcript(events, red: Redactor):
    lines = []
    for ev in events:
        payload = ev.get("payload")
        ts = ev.get("ts", "")
        if isinstance(payload, dict) and "Vendor" in payload:
            vendor = payload["Vendor"]
            frame = vendor.get("json")
            if isinstance(frame, dict) and frame.get("type") == "system" and "skills" in frame:
                frame = {"type": "system", "subtype": frame.get("subtype"), "model": frame.get("model"),
                         "note": "catalogue frame reduced by live-smoke-collect.py"}
            elif isinstance(frame, dict):
                frame = {k: v for k, v in frame.items() if k not in CATALOGUE_KEYS}
            lines.append({"ts": ts, "key": vendor.get("key"), "frame": red.value(frame)})
        else:
            lines.append({"ts": ts, "marion": red.value(payload)})
    if len(lines) > MAX_FRAMES:
        half = MAX_FRAMES // 2
        dropped = len(lines) - MAX_FRAMES
        lines = lines[:half] + [{"elided_frames": dropped}] + lines[-half:]
    return lines


def verify_branch(repo: Path, branch, test_cmd: str, check: str, scratch: Path):
    """Run the real test command and the scenario's check on the landed branch: pass, fail or none."""
    if not branch:
        return "none", "no branch landed\n"
    wt = scratch / "verify"
    add = subprocess.run(["git", "-C", str(repo), "worktree", "add", "--detach", str(wt), branch],
                         capture_output=True, text=True)
    if add.returncode != 0:
        return "fail", f"git worktree add failed: {add.stderr}"
    log = []
    ok = True
    for label, argv in (("test", ["sh", "-c", test_cmd]), ("check", [sys.executable, "-c", check])):
        try:
            r = subprocess.run(argv, cwd=wt, capture_output=True, text=True, timeout=120)
            code, out, err = r.returncode, r.stdout, r.stderr
        except subprocess.TimeoutExpired:
            code, out, err = None, "", "timed out after 120 s"
        log.append(f"$ {label}: exit {code}\n{out}{err}")
        ok = ok and code == 0
    subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(wt)], capture_output=True)
    return ("pass" if ok else "fail"), "\n".join(log)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--state", required=True, type=Path)
    ap.add_argument("--scratch", required=True)
    ap.add_argument("--scenario", required=True)
    ap.add_argument("--expect-child", required=True, help="harness name the child should run on")
    ap.add_argument("--repo", required=True, type=Path, help="the scenario's fixture repository")
    ap.add_argument("--test-cmd", required=True, help="the fixture's real test command, run on the landed branch")
    ap.add_argument("--check", required=True, help="python the scenario adds to the test command")
    ap.add_argument("--wall-secs", required=True, type=int)
    ap.add_argument("--timed-out", default="no")
    ap.add_argument("--steer", default="")
    ap.add_argument("--console", type=Path, help="the root's `marion run` output")
    ap.add_argument("--steer-log", type=Path, help="what the driver's `marion steer` printed")
    ap.add_argument("--out", required=True, type=Path)
    a = ap.parse_args()

    red = Redactor(a.scratch)
    a.out.mkdir(parents=True, exist_ok=True)
    pdir = project_dir(a.state)
    nodes, steers = walk_nodes(pdir)
    by_task = {}
    for path, c in contracts(pdir):
        by_task[c.get("task_id")] = c

    # marion's rendered console, minus the raw frames it echoes (those are in the transcripts).
    if a.console and a.console.exists():
        kept = [l for l in a.console.read_text(errors="replace").splitlines() if not l.startswith("{")]
        (a.out / "console.txt").write_text(red.text("\n".join(kept)) + "\n")
    if a.steer_log and a.steer_log.exists():
        (a.out / "steer.txt").write_text(red.text(a.steer_log.read_text(errors="replace")))

    root = next((n for n in nodes if n["parent_id"] is None), None)
    children = [n for n in nodes if n["parent_id"] is not None]
    for n in nodes:
        events = read_jsonl(pdir / "agents" / n["agent_id"] / "events.jsonl")
        n["cost_usd"] = node_cost_usd(events)
        n["tool_calls"] = tool_calls(events)
        role = "root" if n is root else f"child{children.index(n) + 1}"
        (a.out / f"{role}-{n['harness']}.transcript.json").write_text(
            json.dumps(transcript(events, red), indent=1, ensure_ascii=False) + "\n")

    child_contracts = []
    for c in by_task.values():
        child_contracts.append(c)
        (a.out / f"contract-{c['child']['harness']}.json").write_text(
            json.dumps(red.value(c, cut=False), indent=1, ensure_ascii=False) + "\n")

    wanted = [c for c in child_contracts if c["child"]["harness"] == a.expect_child]
    main_c = wanted[0] if wanted else (child_contracts[0] if child_contracts else None)
    comp = (main_c or {}).get("completion") or {}
    narrative = (comp.get("narrative") or {}).get("value")
    evidence = comp.get("evidence") or []

    verify, verify_log = verify_branch(a.repo, comp.get("branch"), a.test_cmd, a.check, Path(a.scratch))
    (a.out / "verify.log").write_text(red.text(verify_log))

    def fmt_usage(u):
        if not u:
            return None
        return {**u, "total": sum((u.get(k) or 0) for k in ("input", "output", "cache_read", "cache_write"))}

    row = {
        "scenario": a.scenario,
        "root": root and {"harness": root["harness"], "version": root["version"],
                          "usage": fmt_usage(root["usage"]), "cost_usd": root["cost_usd"],
                          "exit": root["exit"], "marion_tools": [t for t in root["tool_calls"] if "marion" in t or t in ("spawn", "wait", "steer", "status", "list")]},
        "delegated": bool(children),
        "children": [{"harness": n["harness"], "agent_type": n["agent_type"], "version": n["version"],
                      "generations": n["generations"], "exit": n["exit"],
                      "usage": fmt_usage(n["usage"]), "cost_usd": n["cost_usd"]} for n in children],
        "child_harness_expected": a.expect_child,
        "child_harness_matches": bool(wanted),
        "child_model": (main_c or {}).get("child", {}).get("model"),
        "reported": bool(main_c) and narrative is not None and not comp.get("narrative_synthesized", True),
        "status": comp.get("status"),
        "failure_cause": comp.get("failure_cause"),
        "narrative": red.text(narrative) if narrative else None,
        "declared_verification": [red.text(" ".join([v.get("program", "")] + v.get("args", []))) for v in (main_c or {}).get("verification", [])],
        "marion_evidence_exit_codes": [e.get("exit_code") for e in evidence],
        "branch_landed": bool(comp.get("branch") and comp.get("commit") and comp.get("changed_paths")),
        "changed_paths": comp.get("changed_paths"),
        "scope_violations": comp.get("scope_violations"),
        "verify": verify,
        "steer": a.steer or None,
        "steer_records": [{"record": s["record"], "detail": red.value(s["detail"])} for s in steers],
        "wall_secs": a.wall_secs,
        "timed_out": a.timed_out == "yes",
    }
    (a.out / "result.json").write_text(json.dumps(row, indent=1, ensure_ascii=False) + "\n")
    json.dump(row, sys.stdout)
    print()


if __name__ == "__main__":
    main()
