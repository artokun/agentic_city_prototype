use std::process::Command;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed=schemas/world.fbs");

    // Generate Rust types
    let status = Command::new("flatc")
        .args(["--rust", "-o", &out_dir, "schemas/world.fbs"])
        .status()
        .expect("flatc not found — install FlatBuffers compiler");
    assert!(status.success(), "flatc --rust failed");

    // Generate TypeScript types for client
    let _ = std::fs::create_dir_all("../client/src/generated");
    let _ = Command::new("flatc")
        .args([
            "--ts",
            "-o",
            "../client/src/generated/",
            "schemas/world.fbs",
        ])
        .status();
}
