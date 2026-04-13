# sherpa-onnx-sys CUDA patch (scratch)

Target: `sherpa-onnx/rust/sherpa-onnx-sys/` in a fork of `k2-fsa/sherpa-onnx`.
Version in scope: 1.12.36 (matches our workspace pin).

## Scope of changes

Only two files touched:

1. `sherpa-onnx/rust/sherpa-onnx-sys/Cargo.toml`
2. `sherpa-onnx/rust/sherpa-onnx-sys/build.rs`

No changes to the high-level `sherpa-onnx` wrapper crate, no changes to
bindings, no changes to C/C++ code. `provider: "cuda"` is a runtime string —
it's already accepted by the existing Rust API. We're only teaching the
sys crate how to link against the CUDA prebuilt instead of the CPU prebuilt.

The `copy_unix_runtime_libs` helper already globs `*.so*` from the `lib/`
directory and copies everything it finds into the target output dir. The
CUDA archive's `libonnxruntime_providers_cuda.so`,
`libonnxruntime_providers_shared.so`, and `libonnxruntime_providers_tensorrt.so`
all live in that same `lib/`, so they come along for free with zero extra code.

The CUDA archive extracts to `sherpa-onnx-v{version}-cuda-12.x-cudnn-9.x-linux-x64-gpu/lib/`.
The existing `archive_stem` → `extracted_dir.join("lib")` math already handles
this pattern (confirmed against the CPU archive layout). No path changes.

---

## Diff 1: `Cargo.toml`

```diff
 [features]
 default = ["static"]
 shared = []
 static = []
+
+# Link against a CUDA + cuDNN prebuilt of sherpa-onnx.
+#
+# Implies `shared` — the CUDA prebuilts are only published as shared
+# libraries; there is no CUDA static archive on the k2-fsa releases page.
+# Explicit combination `static + cuda` is rejected in build.rs with a
+# clear error.
+#
+# Linux x86_64 only today. The CUDA archive on the k2-fsa releases page is
+# pinned to CUDA 12.x + cuDNN 9.x; users are responsible for having those
+# runtime libraries installed on their system (onnxruntime dlopens them).
+cuda = ["shared"]
```

Rationale for `cuda = ["shared"]`: activating cuda without activating shared
would leave `resolve_link_mode` in the Static arm, which has no CUDA archive.
Implying `shared` from `cuda` avoids a footgun and keeps the feature
self-contained for downstream users.

---

## Diff 2: `build.rs`

### Change A — `resolve_link_mode()` (around line 80)

```diff
 fn resolve_link_mode() -> Result<LinkMode, DynError> {
     let static_enabled = env::var_os("CARGO_FEATURE_STATIC").is_some();
     let shared_enabled = env::var_os("CARGO_FEATURE_SHARED").is_some();
+    let cuda_enabled = env::var_os("CARGO_FEATURE_CUDA").is_some();

     if static_enabled && shared_enabled {
         return Err("Features `static` and `shared` cannot be enabled at the same time".into());
     }

+    if cuda_enabled && static_enabled {
+        return Err(
+            "Feature `cuda` requires shared linking; \
+             the k2-fsa CUDA prebuilt is only published as a shared library. \
+             Enable `shared` (or disable `static`) when using `cuda`."
+                .into(),
+        );
+    }
+
     if shared_enabled {
         Ok(LinkMode::Shared)
     } else {
         Ok(LinkMode::Static)
     }
 }
```

Note: when `cuda` is enabled, `shared` is also enabled (via the
`cuda = ["shared"]` feature declaration), so the existing `if shared_enabled`
arm takes care of returning `LinkMode::Shared`. We don't need a `LinkMode::Cuda`
variant — CUDA is a *which archive* question, not a *how to link* question.

### Change B — `archive_name()` (around line 194)

Add a CUDA branch at the top of the match, so it wins over the plain
`(LinkMode::Shared, "linux", "x86_64")` arm when the feature is enabled.
The `target_os` and `target_arch` are passed in; we also need to know
whether `cuda` is on, which means threading it through.

Two options for threading:

- **Option 1**: read `CARGO_FEATURE_CUDA` inside `archive_name`. Simple, no
  signature change, but couples the function to env var reads.
- **Option 2**: pass a `cuda: bool` parameter from `try_main` → `download_prebuilt_libs`
  → `archive_name`. More explicit, threads through one more layer.

I'd go with **Option 1** for minimal diff and to match the existing style
(`resolve_link_mode` already reads env vars directly). Upstream reviewers
tend to prefer minimal surface-area changes.

```diff
 fn archive_name(
     link_mode: LinkMode,
     target_os: &str,
     target_arch: &str,
 ) -> Result<String, DynError> {
     let version = env!("CARGO_PKG_VERSION");
+    let cuda_enabled = env::var_os("CARGO_FEATURE_CUDA").is_some();
+
+    if cuda_enabled {
+        // CUDA prebuilts are shared-only and currently linux-x64 only.
+        // The archive is pinned to CUDA 12.x + cuDNN 9.x; users must have
+        // compatible system libraries (libcudnn.so.9, libcublas.so.12) at
+        // runtime — onnxruntime dlopens them.
+        return match (link_mode, target_os, target_arch) {
+            (LinkMode::Shared, "linux", "x86_64") => Ok(format!(
+                "sherpa-onnx-v{version}-cuda-12.x-cudnn-9.x-linux-x64-gpu.tar.bz2"
+            )),
+            _ => Err(format!(
+                "Feature `cuda` is only supported on linux-x86_64 with shared linking. \
+                 Got: link_mode={link_mode:?}, os={target_os}, arch={target_arch}"
+            )
+            .into()),
+        };
+    }
+
     let name = match (link_mode, target_os, target_arch) {
         (LinkMode::Static, "linux", "x86_64") => {
             format!("sherpa-onnx-v{version}-linux-x64-static-lib.tar.bz2")
         }
         // ... rest of existing match unchanged ...
```

That's the entire upstream patch. **No other file changes.**

---

## Local-fork-only commit (second commit on our branch)

Root-level `Cargo.toml` for cargo-git-dep discovery. This is **not** part of
the upstream PR — cherry-pick only Diffs 1 and 2 onto a clean upstream PR
branch.

Create `/Cargo.toml` at the repo root with:

```toml
# Minimal virtual workspace so cargo git-dependencies can discover
# `sherpa-onnx-sys` and `sherpa-onnx` without needing a `subdir` hint.
#
# NOT FOR UPSTREAM — local-fork-only. See the upstream PR branch for the
# clean build.rs / Cargo.toml changes without this file.
[workspace]
resolver = "2"
members = [
    "sherpa-onnx/rust/sherpa-onnx",
    "sherpa-onnx/rust/sherpa-onnx-sys",
]
```

Commit message: `chore(workspace): add root Cargo.toml for cargo git-dep discovery [local-fork-only, not for upstream]`

---

## Verification (before we wire it into SDR-RS)

On the fork branch, in `sherpa-onnx/rust/`:

```bash
cargo check -p sherpa-onnx-sys --no-default-features --features cuda
```

Expected: build.rs downloads the CUDA archive (~235MB, one-time), extracts
to `target/sherpa-onnx-prebuilt/sherpa-onnx-v1.12.36-cuda-12.x-cudnn-9.x-linux-x64-gpu/lib/`,
links against the libs, succeeds. Warnings from the existing code about
shared runtime lib copy are expected.

Then negative test:

```bash
cargo check -p sherpa-onnx-sys --no-default-features --features "cuda static"
```

Expected: build fails with the new error message about cuda requiring shared.

---

## SDR-RS side — what changes after the fork is ready

Separate PR (`feature/sherpa-cuda`) on our repo, depending on a specific
git rev of your fork:

1. Workspace `Cargo.toml`: replace `sherpa-onnx = "1.12"` with a `git =` dep pointing at your fork + pinned rev.
2. `crates/sdr-transcription/Cargo.toml`: add `sherpa-cuda = ["sherpa", "sherpa-onnx/cuda"]` feature alongside `sherpa-cpu`. The `sherpa-onnx/cuda` passthrough requires `sherpa-onnx`'s own `Cargo.toml` to re-export the feature (may need a micro-patch there too — TBD during implementation).
3. `backends/sherpa/host.rs:421`, `:478`, `:481`: replace hardcoded `"cpu"` with a const that switches on `cfg(feature = "sherpa-cuda")`.
4. `backends/sherpa/silero_vad.rs:72`: same treatment for the VAD provider string.
5. Makefile: document the new build flavor.
6. README.md / CLAUDE.md: document the CUDA 12.x + cuDNN 9.x runtime requirement, install commands for Arch/Ubuntu.
7. CI: add a `sherpa-cuda` build job (cargo check only — runners don't have GPUs, but the link step and archive download still get exercised).
