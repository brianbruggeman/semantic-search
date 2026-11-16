# pinned fastembed patch

Origin: crates.io fastembed 7.5.0, copied from the Cargo registry package. Upstream LICENSE and package metadata are retained.

The local change in src/common.rs sets ONNX Runtime AutoDevicePolicy::PreferCPU before registering explicit execution providers. An empty provider list therefore means CPU; explicit CoreML and other providers still register afterward. The installed ort 2.0.0-rc.15 otherwise defaults to MaxEfficiency, which automatically selects CoreML on macOS.

This patch changes no tokenizer, pooling, model files, batching, or normalization. It is selected through .cargo/config.toml so the registry dependency remains pinned and the policy patch is auditable.
