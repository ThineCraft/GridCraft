# Native and browser replay

The JSON sequences in this directory are embedded byte-for-byte in `tests/wasm_replay.rs`
(a new sequence goes in its `FIXTURES` list too; `embedded_fixtures_match_the_folder` checks).
`native-digests.json` is the desktop reference, not a sequence: each fixture's rejected steps,
both fingerprints and the canonical walk. Regenerate and check it after changing a fixture or
the engine:

```sh
cargo test -p gridcraft-engine --test wasm_replay write_native_digests -- --ignored --nocapture
cargo test -p gridcraft-engine --test wasm_replay native_digests_are_current
```

`../coauthor-maths/` holds a second record: a grid of maths and number-display formulas
(LN, LOG, EXP, POWER, trigonometric and hyperbolic functions and their inverses, TEXT and
`&""` around powers of ten). Its computed values depend on the maths library of the platform
that wrote it (named in the file; currently x86_64 Windows, MSVC), so only its entered content
is checked natively. Every computed difference the browser engine shows against it must be
listed, exactly, in `allowed-differences.json` (cell, formula, arguments, both values and the
distance in ULP); a new or vanished difference fails. The record also holds every formula
cell's displayed text: as `&` and CONCAT convert it (15 significant digits), under General (as a
default-width cell shows it), and under the three formats the grid's TEXT formulas use. Every
text the browser engine shows differently must be listed, exactly, in
`allowed-text-differences.json` (cell, formula, format and both texts), checked the same way.
Not every displayed text agrees: a 1-ULP difference can change the 15-digit text while the
General display agrees (3 cells, 6 texts when this was written). Rewrite the record on the same
platform:

```sh
cargo test -p gridcraft-engine --test wasm_replay write_native_maths -- --ignored --nocapture
```

then run the WebAssembly tests: on failure they print the lists as `allowed-differences.json`
and `allowed-text-differences.json` would hold them.

A regenerated record (`native-digests.json` or `native-maths.json`) is evidence of agreement
only after the WebAssembly tests have passed against it: the native checks make it current
without running the browser engine, and CI does not run the WebAssembly tests.

Run the WebAssembly side under Node (the `wasm-bindgen-test` default):

```sh
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --target wasm32-unknown-unknown -p gridcraft-engine --test wasm_replay
```

The runner must have the same version as the workspace's `wasm-bindgen` crate: look for
`name = "wasm-bindgen"` in `Cargo.lock` (0.2.129 when this was written). CI only checks that
the engine builds for wasm32; these WebAssembly tests are run by hand for now.
