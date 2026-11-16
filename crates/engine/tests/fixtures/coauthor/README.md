# Native and browser replay

The JSON sequences in this directory are embedded byte-for-byte in `tests/wasm_replay.rs`
(a new sequence goes in its `FIXTURES` list too; `embedded_fixtures_match_the_folder` checks).
`native-digests.json` is the desktop reference, not a sequence. Regenerate and check the desktop reference after changing a fixture:

```sh
cargo test -p gridcraft-engine --test wasm_replay write_native_digests -- --ignored --nocapture
cargo test -p gridcraft-engine --test wasm_replay native_digests_are_current
```

Run the same sequences in WebAssembly under Node (the `wasm-bindgen-test` default):

```sh
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --target wasm32-unknown-unknown -p gridcraft-engine --test wasm_replay
```

The runner and the workspace's `wasm-bindgen` crate must have the same version.
