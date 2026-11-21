#!/usr/bin/env python3
"""Validate Cargo.toml wiring without a Rust toolchain."""
import sys, os, re, tomllib
ROOT = sys.argv[1] if len(sys.argv) > 1 else '.'
errors = []
def load(p):
    with open(p, 'rb') as f: return tomllib.load(f)
root_toml = load(os.path.join(ROOT, 'Cargo.toml'))
ws = root_toml.get('workspace', {})
ws_deps = set(ws.get('dependencies', {}).keys())
def check_duplicates(path):
    table, seen = None, set()
    for i, l in enumerate(open(path, encoding='utf-8').read().split('\n'), 1):
        st = l.strip()
        m = re.match(r'^\[\[?([^\]]+)\]?\]$', st)
        if st.startswith('['):
            table, seen = m.group(1) if m else st, set(); continue
        if st and not st.startswith('#') and '=' in st and table:
            key = st.split('=', 1)[0].strip().strip('"')
            if key in seen: errors.append(f'{path}:{i}: duplicate key `{key}` in [{table}]')
            seen.add(key)
def check_ws_usage(path, data):
    for kind in ('dependencies', 'dev-dependencies', 'build-dependencies'):
        for name, spec in (data.get(kind) or {}).items():
            if isinstance(spec, dict) and spec.get('workspace') is True and name not in ws_deps:
                errors.append(f'{path}: {kind} `{name}` uses .workspace but is not in [workspace.dependencies]')
    for tname, tspec in (data.get('target') or {}).items():
        for kind in ('dependencies', 'dev-dependencies'):
            for name in (tspec.get(kind) or {}):
                if (tspec[kind][name] or {}).get('workspace') is True and name not in ws_deps:
                    errors.append(f'{path}: target.{tname}.{kind} `{name}` not in [workspace.dependencies]')
check_duplicates(os.path.join(ROOT, 'Cargo.toml'))
for member in ws.get('members', []):
    mpath, cpath = os.path.join(ROOT, member), os.path.join(ROOT, member, 'Cargo.toml')
    if not os.path.isdir(mpath): errors.append(f'member {member}: directory missing'); continue
    if not os.path.isfile(cpath): errors.append(f'member {member}: no Cargo.toml'); continue
    check_duplicates(cpath)
    data = load(cpath)
    if data.get('package', {}).get('name') != os.path.basename(member):
        errors.append(f'{cpath}: package.name mismatch')
    check_ws_usage(cpath, data)
    src = os.path.join(mpath, 'src')
    if not os.path.isfile(os.path.join(src, 'lib.rs')) and not os.path.isfile(os.path.join(src, 'main.rs')):
        errors.append(f'{member}: no src/lib.rs or src/main.rs')
    for kind in ('dependencies', 'dev-dependencies'):
        for dn, spec in (data.get(kind) or {}).items():
            if isinstance(spec, dict) and 'path' in spec and not os.path.isdir(os.path.normpath(os.path.join(ROOT, spec['path']))):
                errors.append(f'{cpath}: path dep {dn} -> {spec["path"]} missing')
for spec in ws.get('dependencies', {}).values():
    if isinstance(spec, dict) and 'path' in spec and not os.path.isdir(os.path.normpath(os.path.join(ROOT, spec['path']))):
        errors.append(f'workspace path dep -> {spec["path"]} missing')
crates = os.path.join(ROOT, 'crates')
if os.path.isdir(crates):
    for d in sorted(os.listdir(crates)):
        if os.path.isfile(os.path.join(crates, d, 'Cargo.toml')) and f'crates/{d}' not in ws.get('members', []):
            errors.append(f'crates/{d} has a Cargo.toml but is not a workspace member')
print('\n'.join(f'ERROR: {e}' for e in errors) if errors else f'✓ Cargo wiring OK: {len(ws.get("members", []))} members, {len(ws_deps)} workspace deps')
sys.exit(1 if errors else 0)
