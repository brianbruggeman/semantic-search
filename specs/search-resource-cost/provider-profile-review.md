# Provider and profile source review

## Provider path

- `.cargo/config.toml:1-2` patches the crates.io package by the workspace-relative `vendor/fastembed` path. `ss/Cargo.toml:13` retains `fastembed = "7.5.0"`; `Cargo.lock:1293-1302` resolves the patch as a source-free package entry rather than a registry entry.
- `vendor/fastembed/Cargo.toml:15-18,236-242` identifies 7.5.0 and depends on ort `=2.0.0-rc.15` with `api-24`. Its `LICENSE` and normalized `Cargo.toml` match the local Cargo registry package byte for byte. The diff against the registry `src/common.rs` contains only the AutoDevicePolicy import and the `with_auto_device(PreferCPU)` builder call with error mapping.
- `vendor/fastembed/src/common.rs:450-501` builds every ONNX session through `init_session_builder`, placing `PreferCPU` before manual provider registration. Calls from text embedding include `vendor/fastembed/src/text_embedding/init.rs:134` and `vendor/fastembed/src/text_embedding/impl.rs:70`; other model families also use this shared builder.
- The pinned ort source `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ort-2.0.0-rc.15/src/session/builder/impl_options.rs:345-380` states that `PreferCPU` never automatically registers an EP and manual providers take precedence. The same source at `:20-44` registers manually supplied providers in order. `ss/src/embed.rs:623-640` sends an empty provider list for CPU and a CoreML dispatch for the explicit macOS accelerator choices. This supports the source-level policy behavior; actual native allocation remains an AC5 observation.

## Store identity and preservation

- `ss/src/embed.rs:168-184` changes only FastEmbed CPU identity to `cpu-explicit`; CoreML and hash identities retain their names. `ss/src/settings.rs:186-201` hashes that profile identity into a directory under the root hash's `profiles/` child. The old root-level `index.db` path is therefore outside the new `StoreConfig.dir`.
- `ss/src/store.rs:85-99` opens only `config.dir/index.db`; `:142-162` deletes derived rows only in that opened database if its pinned model differs. Thus opening the new profile path cannot invoke the reset against the old root-level database. `ss/src/daemon.rs:120-131` passes the new config into shard creation, and `:178-184` removes only the current profile directory during root cleanup.
- `ss/src/settings.rs:341-383` asserts literal old identity separation, writes hand-authored bytes at the former root-level path, opens the corrected profile through `Store::open`, checks an empty vector store, checks profile-directory distinction, and rereads the unchanged old bytes. This is a direct preservation assertion for the old path. The old bytes are intentionally not a valid SQLite database, so this fixture does not verify that an existing legacy SQLite database can still be opened and queried. The implementation path supports that inference, but a real legacy-database fixture would cover it more directly.

## Open evidence boundary

No native CoreML allocation trace or active accelerator session was inspected in this review. The source establishes the requested builder policy and explicit registration precedence; AC5 is needed for the allocation claim.
