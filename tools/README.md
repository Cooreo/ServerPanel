# Offline check harnesses

CI runs the real toolchain (`cargo fmt`, `clippy`, `cargo test`, `tsc`, `vitest`).
These scripts are for the window *before* a Rust toolchain is available: they catch
the class of mistakes that make a whole crate unparseable or a workspace unloadable,
so a CI run is spent on type errors rather than on a missing brace.

    node tools/rs-check.js crates/sp-core      # tree-sitter parse of every .rs
    python3 tools/dep-check.py .               # Cargo.toml / member / dep sanity

`rs-check.js` needs two npm packages that are deliberately **not** vendored here:

    npm install --prefix tools web-tree-sitter@0.22.6 tree-sitter-wasms@0.1.11

The grammar is loaded from `tools/node_modules/tree-sitter-wasms/out/tree-sitter-rust.wasm`.
A clean parse proves nothing about types — it only proves the file is syntactically
valid Rust. `dep-check.py` is stronger for manifests: it flags a `[workspace.members]`
entry with no `Cargo.toml` (which makes *every* cargo command fail with a 0-second,
log-less CI error), duplicate TOML keys, and dependencies that are not declared in
`[workspace.dependencies]`.
