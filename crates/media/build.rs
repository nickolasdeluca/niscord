//! OpenH264 compiles its SIMD assembly only when `nasm` is on PATH, and
//! silently falls back to plain C otherwise, which encodes 4-6x slower.
//! Make that impossible to miss.

fn main() {
    println!("cargo:rerun-if-env-changed=PATH");
    let found = std::process::Command::new("nasm").arg("-v").output().is_ok_and(|o| o.status.success());
    if !found {
        println!(
            "cargo:warning=nasm not found: OpenH264 will be built without SIMD and encode 4-6x slower. \
             Install it (e.g. `winget install NASM.NASM`), make sure it is on PATH, then run \
             `cargo clean -p openh264-sys2` and rebuild."
        );
    }
}
