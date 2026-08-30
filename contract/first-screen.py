#!/usr/bin/env python3
"""How many channels stand between the port and a recognisable UI?

"Ported 26 of 161" is the wrong denominator for that question: most of the 161
sit behind settings tabs and modals nobody sees on launch. This counts only the
channels the MAIN SCREEN's own call sites reach, so the answer tracks the code
rather than a guess that goes stale.

Run from the repo root:  python3 contract/first-screen.py
"""
import json
import pathlib
import re
import sys
from collections import Counter

ROOT = pathlib.Path(__file__).resolve().parent.parent

# The components that are on screen at launch. Deliberately NOT every file that
# happens to be imported: a settings modal reachable by a click is not the first
# screen, and folding it in would inflate the number back toward 161.
MAIN_SCREEN = [
    "App.tsx",
    "store/store.ts",
    "store/config.ts",
    "hooks/useHive.ts",
    "hooks/usePtyParser.ts",
    "hooks/useRestoreTeam.ts",
    "components/AgentStrip.tsx",
    "components/AgentCard.tsx",
    "components/AgentDetailPanel.tsx",
    "components/AgentControlStrip.tsx",
    "components/MessageQueueComposer.tsx",
    "components/CommandCenterPanel.tsx",
    "components/PtyTerminalView.tsx",
    "components/terminalPool.ts",
    "scene/office/OfficeFloor.tsx",
    "scene/office/Character.ts",
]

# Channels the server actually serves. Parsed from the source so this cannot
# drift: RPC channels from the dispatch table, push channels from the `Push::`
# variants the server constructs. Counting only RPC would understate progress
# the moment the first subscription starts being emitted.
RPC_RS = ROOT / "rust/crates/md-server/src/rpc.rs"
SERVER_SRC = ROOT / "rust/crates/md-server/src"
MANIFEST = ROOT / "contract/manifest.json"
SRC = ROOT / "src/renderer/src"

CALL_RE = re.compile(r"\bcth\.([A-Za-z_][A-Za-z0-9_]*)")
VARIANT_RE = re.compile(r"Rpc::(\w+) => Op::")
PUSH_RE = re.compile(r"Push::(\w+)")


def ported_channels(by_variant):
    if not RPC_RS.exists():
        sys.exit(f"missing {RPC_RS}")
    src = RPC_RS.read_text()
    served = set(VARIANT_RE.findall(src))

    # Channels the server deliberately will NOT port (clipboard, the desktop
    # updater, the app's own window) are resolved, not pending. Counted as gaps
    # they would keep the number from ever reaching zero and hide the real work.
    # Read from the `plan` function's own arms, so this cannot drift from it.
    head, _, rest = src.partition("pub const fn plan(")
    never_region, _, _ = rest.partition("other => match handler_for")
    served |= set(re.findall(r"Rpc::(\w+)", never_region))
    del head

    # A push channel counts as served where the server emits it. `.as_str()` and
    # the enum definition itself are not emissions, so skip the contract crate.
    for f in SERVER_SRC.rglob("*.rs"):
        served |= set(PUSH_RE.findall(f.read_text()))
    return {by_variant[v] for v in served if v in by_variant}


def methods_in(paths):
    found = set()
    for p in paths:
        if p.exists():
            found |= set(CALL_RE.findall(p.read_text()))
    return found


def main():
    man = json.loads(MANIFEST.read_text())
    by_method = {m["name"]: m for m in man["methods"]}

    # manifest channel -> the Rust enum variant build.rs generates for it, so a
    # rename on either side shows up as a miss rather than a silent zero.
    def variant(ch):
        return "".join(w[:1].upper() + w[1:] for w in re.split(r"[:\-.]", ch) if "{" not in w)

    by_variant = {variant(c): c for m in man["methods"] for c in m["channels"]}
    ported = ported_channels(by_variant)

    def channels(methods):
        out = set()
        for name in methods:
            if name in by_method:
                out |= set(by_method[name]["channels"])
        return out

    core = channels(methods_in(SRC / f for f in MAIN_SCREEN))
    whole = channels(methods_in(p for p in SRC.rglob("*") if p.suffix in (".ts", ".tsx")))

    print(f"resolved (done+never)  : {len(ported)} channels")
    print(f"main screen needs      : {len(core)} channels, {len(core - ported)} unported")
    print(f"whole renderer needs   : {len(whole)} channels, {len(whole - ported)} unported")
    print("\nmain-screen gap by namespace:")
    for ns, n in Counter(c.split(":")[0] for c in core - ported).most_common():
        print(f"  {ns:<12} {n}")
    print("\nmain-screen gap:")
    for c in sorted(core - ported):
        print(f"  {c}")


if __name__ == "__main__":
    main()
