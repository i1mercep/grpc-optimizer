fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/optimization/v1/optimizer.proto");
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .message_attribute(".", "#[serde(default)]")
        .file_descriptor_set_path(
            std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("optimizer.bin"),
        )
        .compile_protos(&["proto/optimization/v1/optimizer.proto"], &["proto"])?;
    Ok(())
}
