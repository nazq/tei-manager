fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::env;
    use std::path::PathBuf;

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    // Compile both protos with file descriptor set for reflection
    // This generates both the code and the descriptor for grpcurl/reflection
    // The include path is specified as the second argument to compile_protos
    tonic_prost_build::configure()
        .file_descriptor_set_path(out_dir.join("descriptor.bin"))
        .compile_protos(
            &[
                "proto/tei/v1/tei.proto",
                "proto/tei_multiplexer/v1/multiplexer.proto",
            ],
            &["proto"],
        )?;

    Ok(())
}
