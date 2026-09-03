fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/resolver.proto");
    tonic_build::configure()
        .build_client(true)
        .build_server(true) // the server stub is used by this crate's own tests
        .compile_protos(&["proto/resolver.proto"], &["proto"])?;
    Ok(())
}
