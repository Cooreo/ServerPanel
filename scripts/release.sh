#!/usr/bin/env bash
# Cut a release, consistently.
#
# The version lives in four places (Cargo workspace, web/package.json,
# tauri.conf.json, and the tag). Hand-editing them is how you get a release where
# the About box disagrees with the installer filename, so this script is the only
# supported way to produce one.
#
#   scripts/release.sh 0.1.0            # normal release
#   scripts/release.sh 0.1.0-rc.1       # pre-release (GitHub marks it prerelease)
#   DRY_RUN=1 scripts/release.sh 0.1.0  # show what would change
set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  echo "usage: $0 <version>   (e.g. 0.1.0 or 0.1.0-rc.1)" >&2
  exit 2
fi
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: '$VERSION' is not a semver version (MAJOR.MINOR.PATCH[-prerelease])" >&2
  exit 2
fi

cd "$(dirname "$0")/.."
ROOT="$PWD"
DRY_RUN="${DRY_RUN:-0}"

run() {
  echo "+ $*"
  if [[ "$DRY_RUN" != "1" ]]; then "$@"; fi
}

# --- cleanliness -------------------------------------------------------------
[[ -z "$(git status --porcelain)" ]] || { echo "error: working tree is not clean" >&2; exit 1; }
git fetch --quiet origin
if ! git diff --quiet origin/main -- .; then
  echo "error: local branch differs from origin/main; merge or rebase first" >&2
  exit 1
fi

# --- toolchain gate --------------------------------------------------------
command -v cargo >/dev/null || { echo "error: cargo not found" >&2; exit 1; }
echo "→ fmt / clippy / test"
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
if [[ -f web/package.json ]]; then
  echo "→ web checks"
  ( cd web && npm ci --no-audit --no-fund && npx tsc --noEmit && npx eslint . --max-warnings 0 && npx vitest run && npm run build )
fi

# --- version bump ----------------------------------------------------------
echo "→ setting version to $VERSION"
python3 - "$VERSION" <<'PY'
import json, re, sys, pathlib
v = sys.argv[1]
root = pathlib.Path('.')

# 1. workspace manifest (single source of truth for every crate)
p = root / 'Cargo.toml'
t = p.read_text()
new, n = re.subn(r'(?m)^version = "[^"]*"$', f'version = "{v}"', t, count=1)
assert n == 1, 'no [workspace.package] version line found'
p.write_text(new)

# 2. web/package.json
pkgp = root / 'web' / 'package.json'
if pkgp.exists():
    data = json.loads(pkgp.read_text())
    data['version'] = v
    pkgp.write_text(json.dumps(data, indent=2) + '\n')

# 3. tauri.conf.json (Tauri v2 reads version from here for the bundle)
for cand in ('crates/sp-desktop/tauri.conf.json', 'crates/sp-desktop/src-tauri/tauri.conf.json'):
    tp = root / cand
    if tp.exists():
        data = json.loads(tp.read_text())
        data['version'] = v
        tp.write_text(json.dumps(data, indent=2) + '\n')
        print(f'   updated {cand}')
        break

# 4. Cargo.lock so a CI checkout never re-resolves the version
if (root / 'Cargo.lock').exists():
    import subprocess
    subprocess.run(['cargo', 'update', '-p', 'sp-core', '--workspace'], check=False)
PY

# --- changelog -------------------------------------------------------------
if grep -q "^## \[Unreleased\]" CHANGELOG.md 2>/dev/null; then
  echo "→ promoting the Unreleased section to [$VERSION]"
  python3 - "$VERSION" <<'PY'
import datetime, pathlib, re, sys
v = sys.argv[1]
today = datetime.date.today().isoformat()
p = pathlib.Path('CHANGELOG.md')
t = p.read_text()
if re.search(rf"^## \[{re.escape(v)}\]", t, re.M):
    raise SystemExit(f'CHANGELOG.md already has a [{v}] section — edit it instead of running this again')
t = t.replace(
    '## [Unreleased]',
    f'## [Unreleased]\n\n## [{v}] - {today}',
    1,
)
p.write_text(t)
PY
else
  echo "warn: CHANGELOG.md has no [Unreleased] heading; add the $VERSION section by hand" >&2
fi

# --- commit, tag, push -----------------------------------------------------
files=(Cargo.toml Cargo.lock CHANGELOG.md)
[[ -f web/package.json ]] && files+=(web/package.json)
for cand in crates/sp-desktop/tauri.conf.json crates/sp-desktop/src-tauri/tauri.conf.json; do
  [[ -f "$cand" ]] && files+=("$cand")
done
run git add "${files[@]}"
run git commit -m "chore(release): v$VERSION"
run git tag -a "v$VERSION" -m "ServerPanel v$VERSION"
run git push origin HEAD
run git push origin "v$VERSION"

cat <<DONE

Tagged v$VERSION. .github/workflows/release.yml picks the tag up and:
  1. checks the version matches the manifest (this script guarantees it),
  2. drafts a release whose notes come from CHANGELOG.md,
  3. builds + uploads the Windows NSIS/MSI installers, Linux AppImage/deb and the
     macOS dmg, plus the headless server binary per platform,
  4. publishes the release only if at least one installer was produced.

Watch it at: https://github.com/Cooreo/ServerPanel/actions/workflows/release.yml
DONE
