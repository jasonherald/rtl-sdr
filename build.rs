// Workspace root binary build script.
//
// The only job here is to make `sherpa-cuda` builds actually runnable
// after `make install`. The `sherpa-onnx-sys` build script tries to
// emit `-Wl,-rpath,$ORIGIN` on its own, but `cargo:rustc-link-arg`
// only affects the link step of the crate that emits it — and the sys
// crate builds an rlib, so the link arg never reaches the final
// binary. Without an rpath hint the dynamic loader has no way to find
// the sherpa-onnx / onnxruntime shared libraries sitting next to the
// installed binary, and you get:
//
//     error while loading shared libraries: libsherpa-onnx-c-api.so:
//     cannot open shared object file: No such file or directory
//
// The fix is to have the BINARY crate (this one) inject the rpath at
// its own link step. `$ORIGIN` is resolved by the loader to the
// directory of the running executable, so as long as the `.so` files
// live either next to the binary (the cargo target/release layout
// that `cargo run` uses) or in an adjacent `sdr-rs-libs/` subdirectory
// (the `make install` layout that keeps `~/.cargo/bin/` uncluttered),
// everything Just Works.
//
// Multi-entry rpath with `:` separator is standard ELF semantics. The
// loader walks entries left to right; the first one catches cargo run
// builds, the second one catches installed builds.
//
// This is feature-gated so static-linked builds (`sherpa-cpu`,
// `whisper-*`) get no extra link args — they don't need an rpath
// because all the C++ code is linked into the binary directly.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SHERPA_CUDA");

    // Read build *target* state from cargo env vars rather than
    // `#[cfg(target_os = ...)]` / `#[cfg(feature = ...)]`. The `cfg`
    // form would reflect the HOST that's running `build.rs`, not the
    // architecture we're compiling for — wrong for any future
    // cross-compilation setup. CARGO_CFG_TARGET_OS and
    // CARGO_FEATURE_* are the canonical cargo-provided values for
    // the build-target state.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let sherpa_cuda = std::env::var_os("CARGO_FEATURE_SHERPA_CUDA").is_some();

    if sherpa_cuda && target_os == "linux" {
        // `$ORIGIN` must reach `ld` as a literal dollar sign; cargo
        // passes `rustc-link-arg` values straight through without
        // shell expansion, so we escape nothing here.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/sdr-rs-libs");

        // Use the old-style DT_RPATH tag instead of the modern
        // DT_RUNPATH. This matters because onnxruntime calls
        // `dlopen("libonnxruntime_providers_cuda.so")` at recognizer
        // creation time, and the provider .so's own NEEDED entries
        // (libcublasLt.so.12, libcudnn.so.9, etc.) then have to be
        // resolved by the dynamic loader.
        //
        // DT_RUNPATH (the default modern tag) is ONLY consulted when
        // resolving the direct NEEDED deps of the ELF object that
        // declares it; it does not cascade to libraries opened via
        // dlopen or to their transitive NEEDED entries. DT_RPATH on
        // the executable, by contrast, IS consulted for every library
        // load regardless of how it was triggered, which is exactly
        // the behavior we want: sdr-rs-libs/ contains both the
        // directly-linked sherpa+onnxruntime libs and the CUDA
        // runtime libs that onnxruntime's dlopen'd providers need,
        // and we want a single rpath entry to cover both.
        //
        // `--disable-new-dtags` is widely supported on GNU ld and
        // lld. DT_RPATH is deprecated for new ELF but still honored
        // by every glibc-based loader, so the ergonomic cost of
        // using it here is basically nil.
        println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags");
    }
}
