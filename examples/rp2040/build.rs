fn main() {
    // memory.x describes the RP2040's flash and RAM; the linker scripts come from cortex-m-rt, embassy-rp and defmt.
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x"); // puts the second-stage boot loader at the start of flash
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    println!("cargo:rerun-if-changed=memory.x");
}
