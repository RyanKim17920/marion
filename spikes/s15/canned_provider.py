#!/usr/bin/env python3
"""Canned OpenAI-Responses provider for spike S15 (detached-supervisor process-group containment).

S15 plays exactly the role S7 played, with the supervisor detached, and its
provider was a byte-for-byte copy of `spikes/s7/canned_provider.py` apart
from the `S15_*` spelling of its environment and two defaults. This file is
now that mapping and nothing else: it translates the `S15_*` variables the S15
probe sets into the `S7_*` ones the S7 provider reads, then runs the S7
provider in place. Turn 1's `custom_tool_call`, turn 2's deliberate hold and
the request log are therefore S7's, unchanged.

No model is called and no API key is used.
"""
import os
import runpy

HERE = os.path.dirname(os.path.abspath(__file__))
S7_PROVIDER = os.path.join(HERE, os.pardir, "s7", "canned_provider.py")

# Defaults S15 chose for itself so it could run beside an S7 provider.
_S15_DEFAULTS = {
    "REQLOG": "/tmp/s15-requests.jsonl",
    "PORT": "8099",
}

for name in ("REQLOG", "PORT", "SPAWN_SH", "PIDFILE", "RUNAWAY_SH",
             "RUNAWAY_PIDFILE", "HOLD_SECS"):
    value = os.environ.get("S15_" + name, _S15_DEFAULTS.get(name))
    if value is not None:
        os.environ["S7_" + name] = value

if __name__ == "__main__":
    runpy.run_path(S7_PROVIDER, run_name="__main__")
