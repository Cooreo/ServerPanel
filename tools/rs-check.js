#!/usr/bin/env node
/* Parse-only Rust checker (no cargo in this sandbox).
 * usage: node rs-check.js [path]   path = file, dir, or repo root
 * Reports syntax errors via tree-sitter. Cannot see missing files/deps/types.
 */
const fs = require('fs');
const path = require('path');
// Resolve the grammar relative to this script so the harness works wherever the repo
// is checked out; TOOLING_MODULES overrides it (e.g. a shared install).
const MODS = [process.env.TOOLING_MODULES, path.join(__dirname, 'node_modules'), '/home/user/.tooling/node_modules'].filter(Boolean);
function req(name) {
  for (const m of MODS) {
    try { return require(path.join(m, name)); } catch (e) {}
  }
  console.error(`missing ${name} — run: npm install --prefix tools web-tree-sitter@0.22.6 tree-sitter-wasms@0.1.11`);
  process.exit(2);
}
const Parser = req('web-tree-sitter');

const root = process.argv[2] || '.';
function collect(p) {
  const st = fs.statSync(p);
  if (st.isFile()) return p.endsWith('.rs') ? [p] : [];
  return fs.readdirSync(p, { withFileTypes: true }).flatMap((d) => {
    if (d.name === 'target' || d.name === '.git' || d.name === 'node_modules') return [];
    return collect(path.join(p, d.name));
  }).sort();
}
const files = collect(path.resolve(root)).filter((f) => f.endsWith('.rs'));
if (!files.length) { console.log('no .rs files found'); process.exit(1); }

(async () => {
  try {
  const WASM_DIR = path.join(MODS.find((m) => fs.existsSync(path.join(m, 'web-tree-sitter'))) || MODS[0], 'web-tree-sitter') + path.sep;
  await Parser.init({ locateFile: (f) => WASM_DIR + f });
  const parser = new Parser();
  const wasm = MODS.map((m) => path.join(m, 'tree-sitter-wasms', 'out', 'tree-sitter-rust.wasm')).find((p) => fs.existsSync(p));
  if (!wasm) { console.error('tree-sitter-rust.wasm not found'); process.exit(2); }
  const Rust = await Parser.Language.load(wasm);
  parser.setLanguage(Rust);
  let bad = 0;
  for (const f of files) {
    // 0.22.x accepts a string (or a progressive-parse callback), not a Buffer.
    const src = fs.readFileSync(f, 'utf8');
    const tree = parser.parse(src);
    const errs = [];
    const walk = (n) => {
      if (n.type === 'ERROR' || n.type === 'MISSING') {
        errs.push(`${n.startPosition.row + 1}  ${(n.namedChildren || []).length ? '' : 'ERROR '}${snippet(src, n.startIndex, n.endIndex)}`);
      }
      for (const c of n.children) if (c) walk(c);
    };
    walk(tree.rootNode);
    const rel = path.relative(path.resolve(root), f) || path.basename(f);
    if (errs.length) {
      bad++;
      console.log(`\x1b[31m✗ ${rel}\x1b[0m`);
      console.log(errs.slice(0, 6).map((e) => `     ${e}`).join('\n'));
      if (errs.length > 6) console.log(`     … ${errs.length - 6} more`);
    }
    tree.delete();
  }
  if (bad === 0) console.log(`\x1b[32m✓ ${files.length} Rust files parse cleanly\x1b[0m`);
  else console.log(`\x1b[31m${bad}/${files.length} files have syntax errors\x1b[0m`);
  process.exit(bad ? 1 : 0);
  } catch (e) {
    console.error('rs-check failed:', String(e && e.message || e).slice(0, 400));
    process.exit(2);
  }
})();

function snippet(src, a, b) {
  const s = src.slice(a, Math.min(b, a + 90)).replace(/\s+/g, ' ').trim();
  return s.length ? s.slice(0, 90) : '<empty>';
}
