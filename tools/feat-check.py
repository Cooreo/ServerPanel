#!/usr/bin/env python3
"""Validate every `features = [...]` against the crates.io index.

Why: a feature name a crate does not have is not a manifest error. It surfaces as
`error: failed to select a version for reqwest. versions that meet the requirements
^0.12 are: 0.12.28, ...` — the resolver reports an unsatisfiable *version*, not the
bad flag — and it fails before a single crate is compiled, so a workspace-wide CI run
produces no compiler diagnostics at all. `reqwest` had a `proxy` feature listed for
exactly this reason; it does not exist (`windows_proxy` does).

    python3 tools/feat-check.py [--strict]

`--strict` (used in CI) also fails when a lookup cannot be completed; without it a
network problem is a warning, because a checker that fails CI because it could not
reach the index is worse than no checker.
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
import urllib.request

ROOT = "Cargo.toml"
CRATES_GLOB = "crates/*/Cargo.toml"


def index_path(name: str) -> str:
    n = len(name)
    if n == 1:
        return f"1/{name}"
    if n == 2:
        return f"2/{name}"
    if n == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[0:2]}/{name[2:4]}/{name}"


_cache: dict[str, list[dict]] = {}


def fetch_lines(name: str, strict: bool) -> list[dict]:
    if name in _cache:
        return _cache[name]
    path = index_path(name)
    text = ""
    try:
        with urllib.request.urlopen(f"https://index.crates.io/{path}", timeout=15) as r:
            text = r.read().decode()
    except Exception as exc:  # network or blocked host -> try the GitHub mirror
        if strict:
            first = f"{type(exc).__name__}: {exc}"
        else:
            first = ""
        try:
            out = subprocess.run(
                [
                    "gh", "api", "-H", "Accept: application/vnd.github.raw+json",
                    f"repos/rust-lang/crates.io-index/contents/{path}?ref=master",
                ],
                capture_output=True, text=True, timeout=60,
            )
            if out.returncode == 0:
                text = out.stdout
            elif strict and first:
                print(f"? index lookup failed for {name}: {first}; mirror said {out.stderr.strip()[:120]}")
        except Exception as exc2:
            if strict:
                print(f"? index lookup failed for {name}: {first or exc2}")
    rows = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    _cache[name] = rows
    return rows


def semver_tuple(v: str) -> tuple:
    core = re.match(r"\s*(\d+)\.(\d+)\.(\d+)", v)
    if core:
        return tuple(int(x) for x in core.groups())
    nums = re.findall(r"\d+", v)
    return tuple(int(x) for x in nums[:3]) + (0,) * (3 - len(nums[:3]))


def prefix_for(pin: str) -> str:
    """`"0.12"` pins a minor (semver treats 0.x specially); `"1"` pins a major."""
    parts = re.findall(r"\d+", pin)
    if not parts:
        return ""
    if parts[0] == "0" and len(parts) > 1:
        return f"{parts[0]}.{parts[1]}."
    return f"{parts[0]}."


def features_of(name: str, pin: str, strict: bool):
    rows = [r for r in fetch_lines(name, strict) if not r.get("yanked")]
    pre = prefix_for(pin)
    cand = [r for r in rows if r["vers"].startswith(pre)] if pre else rows
    if not cand:
        return None, []
    cand.sort(key=lambda r: semver_tuple(r["vers"]))
    best = cand[-1]
    feats = set(best.get("features", {})) | set(best.get("features2", {}))
    # An optional dependency creates an implicit feature named after it (chrono's
    # `serde` support works this way and is not in the features map).
    for d in best.get("deps", []):
        if d.get("optional"):
            feats.add(d.get("rename") or d["name"])
    return best["vers"], sorted(feats)


DEP_LINE = re.compile(r"^([A-Za-z0-9_-]+)\s*=\s*(.*)$")


def _logical_lines(text: str):
    """Yield (section, "name = value") with inline tables joined.

    `windows = { version = "0.58", features = [\n  "Win32_Foundation",\n] }` is one
    dependency written over six lines; a line-by-line scan would see `features = [`
    as its own statement and lose the names.
    """
    section = None
    buf = None
    depth = 0
    for raw in text.splitlines():
        line = raw.strip()
        if buf is not None:
            buf += " " + line
            depth += line.count("{") + line.count("[") - line.count("}") - line.count("]")
            if depth <= 0:
                yield section, buf
                buf, depth = None, 0
            continue
        if not line or line.startswith("#"):
            continue
        if line.startswith("["):
            section = line.strip("[]").strip()
            continue
        m = DEP_LINE.match(line)
        if not m:
            continue
        name, value = m.group(1), m.group(2)
        # strip trailing comments
        value = value.split(" #")[0].rstrip()
        depth = value.count("{") + value.count("[") - value.count("}") - value.count("]")
        if depth > 0 or value.endswith(("{", "[")):
            buf, depth = f"{name} = {value}", depth
            continue
        yield section, f"{name} = {value}"


def features_of_value(value: str) -> list[str]:
    """Only the strings inside `features = [ ... ]` (or a bare `features = [...]`)."""
    m = re.search(r"features\s*=\s*\[([^\]]*)\]", value)
    if not m:
        return []
    return re.findall(r'"([^"]+)"', m.group(1))


def parse_manifest(text: str, root_pins: dict | None):
    """Yield (crate, pin, [features]) for every dependency with explicit features."""
    out = []
    for section, stmt in _logical_lines(text):
        if not section or not section.endswith("dependencies"):
            continue
        m = DEP_LINE.match(stmt)
        if not m:
            continue
        name, value = m.group(1), m.group(2)
        feats = [f for f in features_of_value(value) if not f.startswith(("dep:", "dep/"))]
        # A feature can be spelled `foo?/bar` (optional-dep syntax); those are the
        # dependency's own features and are not validated here.
        feats = [f for f in feats if "?" not in f and "/" not in f]
        pin_m = re.search(r'version\s*=\s*"([^"]+)"', value)
        pin = pin_m.group(1) if pin_m else None
        if pin is None:
            bare = re.match(r'^"([^"]+)"$', value.strip())
            if bare:
                pin = bare.group(1)
        if "workspace = true" in value:
            # Features listed here are *added* to the workspace ones, so they are
            # checked against the workspace's pin.
            pin = None if root_pins is None else root_pins.get(name)
        if pin and feats:
            out.append((name, pin, feats))
    return out


def main(argv: list[str]) -> int:
    strict = "--strict" in argv
    base = "crates"
    root_text = open(ROOT).read()
    ws = re.search(r"\[workspace\.dependencies\](.*?)(\n\[|\Z)", root_text, re.S)
    ws_text = ws.group(1) if ws else ""
    root_entries = parse_manifest("[dependencies]\n" + ws_text, None)
    root_pins = {}
    for name, pin, _ in root_entries:
        root_pins[name] = pin
    import glob as _g
    members = list(root_pins)  # checked below
    targets = [("workspace", None, root_entries)]
    for path in sorted(_g.glob(CRATES_GLOB)):
        text = open(path).read()
        targets.append((path, root_pins, parse_manifest(text, root_pins)))
    problems = 0
    checked = 0
    for where, pins, entries in targets:
        for name, pin, feats in entries:
            checked += 1
            version, all_feats = features_of(name, pin, strict)
            if version is None:
                msg = f"{where}: {name} {pin}: no index data (skipped)"
                if strict:
                    print(f"✗ {msg} — strict mode cannot verify the feature names")
                    problems += 1
                else:
                    print(f"? {msg}")
                continue
            missing = [f for f in feats if f not in all_feats]
            if missing:
                problems += 1
                print(f"✗ {where}: {name} {version} has no feature {missing}")
                print(f"    it has: {', '.join(all_feats[:24])}")
    if problems == 0:
        print(f"✓ {checked} dependency declarations have valid feature names")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
