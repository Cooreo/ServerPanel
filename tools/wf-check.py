#!/usr/bin/env python3
"""Workflow sanity checks that the YAML parser does not perform.

Why this exists: ci.yml carried `name: Test (${{ matrix.os) }})` — a stray paren
inside an expression. The YAML is perfectly valid, so `python -c yaml.load` passes;
the Actions *expression* parser rejects it, and GitHub's response is the worst
possible failure mode: a run that completes in 0 seconds with zero jobs, no log,
and nothing in the API for a token without checks:read. Diagnosing it cost four
pushes.

    python3 tools/wf-check.py .github/workflows

Checks, each mapped to a real failure this repo has hit or could hit:
  * duplicate mapping keys (YAML tolerates them; Actions does not),
  * `${{ }}` delimiters balanced and paired,
  * no unclosed paren / quote / bracket inside an expression,
  * `needs:` referencing a job that exists in the same file,
  * job-level `if:` not using `hashFiles()` (legal only on steps),
  * every job has `runs-on` or is a reusable-workflow call.
"""
import re
import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - the sandbox has pyyaml; CI installs it
    print("pyyaml is required: pip install pyyaml", file=sys.stderr)
    raise SystemExit(2)


def no_dupes(loader, node, deep=False):
    seen, out = set(), {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in seen:
            raise ValueError(
                f"duplicate key {key!r} on line {key_node.start_mark.line + 1}"
            )
        seen.add(key)
        out[key] = loader.construct_object(value_node, deep=deep)
    return out


class Strict(yaml.SafeLoader):
    pass


Strict.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, no_dupes)

EXPR = re.compile(r"\$\{\{.*?\}\}", re.S)
PAIRS = {")": "(", "]": "[", "}": "{"}


def check_expr(expr: str, where: str, errs: list) -> None:
    depth = []
    quote = None
    i = 0
    while i < len(expr):
        c = expr[i]
        if quote:
            if c == "\\":
                i += 2
                continue
            if c == quote:
                quote = None
        elif c in "'\"":
            quote = c
        elif c in "([{":
            depth.append(c)
        elif c in ")]}":
            if not depth or depth[-1] != PAIRS[c]:
                errs.append(f"{where}: unbalanced {c!r} in {expr!r}")
                return
            depth.pop()
        elif c == "$" and expr[i : i + 2] == "${":
            errs.append(f"{where}: nested ${{{{...}}}} is not supported")
            return
        i += 1
    if quote:
        errs.append(f"{where}: unterminated {quote} in {expr!r}")
    if depth:
        errs.append(f"{where}: unclosed {depth[-1]!r} in {expr!r}")


def walk(node, path, errs):
    if isinstance(node, dict):
        for k, v in node.items():
            if isinstance(v, str):
                for m in EXPR.finditer(v):
                    inner = m.group(0)[4:-2].strip()
                    check_expr(inner, f"{path}.{k}", errs)
                    if "hashFiles" in inner and path.endswith(".if") and path.count(".") == 2:
                        errs.append(
                            f"{path}: hashFiles() is only allowed in a step's if, "
                            "not a job's (the job `if` context is github/needs/vars/inputs)"
                        )
                if "${{" in v and v.count("${{") != v.count("}}"):
                    errs.append(f"{path}.{k}: unbalanced ${{{{ / }}, found {v!r}")
            walk(v, f"{path}.{k}", errs)
    elif isinstance(node, list):
        for n, item in enumerate(node):
            walk(item, f"{path}[{n}]", errs)


def check_file(path: Path) -> list:
    errs = []
    text = path.read_text()
    # Raw-delimiter scan runs first: it catches a mangled `${{ x) }}` even where
    # the surrounding YAML would make the structured walk miss it.
    for m in re.finditer(r"\$\{\{", text):
        end = text.find("}}", m.end())
        if end == -1 or "\n" in text[m.end() : end]:
            errs.append(f"line {text[: m.start()].count(chr(10)) + 1}: expression never closes")
    for m in EXPR.finditer(text):
        inner = m.group(0)[4:-2]
        if inner.count("(") != inner.count(")"):
            errs.append(f"{path.name}: unbalanced parentheses in `{m.group(0)}`")
    try:
        doc = yaml.load(text, Loader=Strict)
    except Exception as exc:
        errs.append(f"{path.name}: {exc}")
        return errs
    if not isinstance(doc, dict):
        errs.append(f"{path.name}: not a mapping")
        return errs
    walk(doc, path.stem, errs)
    jobs = doc.get("jobs") or {}
    for name, job in jobs.items():
        if not isinstance(job, dict):
            errs.append(f"{name}: job must be a mapping")
            continue
        if "uses" not in job and "runs-on" not in job:
            errs.append(f"{name}: needs `runs-on` (or `uses:` for a reusable workflow)")
        needs = job.get("needs")
        wanted = [needs] if isinstance(needs, str) else (needs or [])
        for dep in wanted:
            if dep not in jobs:
                errs.append(f"{name}: needs unknown job {dep!r}")
        for out_key, ref in (job.get("outputs") or {}).items():
            for mm in re.finditer(r"needs\.([A-Za-z0-9_-]+)\.", str(ref)):
                if mm.group(1) not in wanted:
                    errs.append(f"{name}.outputs.{out_key}: reads job {mm.group(1)} it does not need")
    return errs


def main(argv: list) -> int:
    roots = [Path(a) for a in (argv[1:] or [".github/workflows"])]
    files = sorted(
        {p for r in roots for p in (r.glob("*.yml") if r.is_dir() else [r])}
        | {p for r in roots for p in (r.glob("*.yaml") if r.is_dir() else [])}
    )
    if not files:
        print("no workflow files found")
        return 1
    bad = 0
    for f in files:
        errs = check_file(f)
        if errs:
            bad += 1
            print(f"\x1b[31m✗ {f}\x1b[0m")
            for e in errs:
                print(f"   {e}")
    if not bad:
        print(f"\x1b[32m✓ {len(files)} workflow files pass expression/needs checks\x1b[0m")
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
