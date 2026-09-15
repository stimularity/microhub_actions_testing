use std::path::PathBuf;

fn main() {
  // The Windows build compiles the runtime payload into the binary with
  // include_bytes!, so the file has to exist at compile time. CI stages the
  // real one; this keeps a plain checkout compiling with an empty placeholder,
  // which ensure_runtime treats as "no bundled runtime".
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
    let payload = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("resources")
      .join("payload.tar.gz");

    if !payload.exists() {
      if let Some(dir) = payload.parent() {
        let _ = std::fs::create_dir_all(dir);
      }
      let _ = std::fs::write(&payload, b"");
    }

    println!("cargo:rerun-if-changed={}", payload.display());
  }

  // Lets CI stamp the payload's identity in without touching any file.
  println!("cargo:rerun-if-env-changed=MICROHUB_RUNTIME_ID");

  tauri_build::build()
}
