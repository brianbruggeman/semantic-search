# Profile storage implementation notes

The store identity for `EmbedderKind::FastEmbed` now names explicitly selected CPU execution as `cpu-explicit`; other provider labels remain the labels returned by `Provider::name()`. This keeps the former literal identity `bge-small-en-v1.5+cpu+len256` distinct from vectors produced under the corrected provider policy. Hash embeddings retain their model-only identity.

`Settings::store_config` continues hashing the root into `home/roots/<root-hash>`, then hashes the computed store identity into `profiles/<profile-hash>`. Thus each root/profile pair gets an isolated `index.db` while the former root-level `index.db` remains at its existing path and is never opened, migrated, or deleted by the corrected profile configuration.

Evidence pointers read during this change:

- `ss/src/settings.rs:186-202`: previous store configuration pointed directly at `home/roots/<root-hash>` and used the profile identity as `model_id`.
- `ss/src/store.rs:87-91`: `Store::open` creates the configured directory and opens only `<config.dir>/index.db`; it pins the configured model identity afterward.
- `ss/src/embed.rs:168-185`: profile identity now maps CPU to `cpu-explicit`, preserves other provider labels, and preserves the hash embedder's model-only identity.
- `ss/src/settings.rs:339-391`: authored profile checks cover the literal legacy identity, unchanged bytes in the legacy root-level file, distinct profile directories, direct filesystem ownership, and a closed-channel control.

No migration or deletion is part of this slice. Existing direct callers of `Store::open` retain their configured directory behavior. The application settings path changes only where its `StoreConfig.dir` points.
