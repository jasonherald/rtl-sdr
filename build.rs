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
// are copied alongside the installed binary (see the `install` target
// in Makefile for the sherpa-cuda branch), everything Just Works.
//
// This is feature-gated so static-linked builds (`sherpa-cpu`,
// `whisper-*`) get no extra link args — they don't need an rpath
// because all the C++ code is linked into the binary directly.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(all(feature = "sherpa-cuda", target_os = "linux"))]
    {
        // `\$ORIGIN` must reach ld as a literal dollar sign; cargo
        // passes the value straight through, so we escape nothing here.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}
