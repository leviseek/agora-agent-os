//! Protobuf/gRPC code generation.
//!
//! protoc is located in this order: the PROTOC environment variable, then the vendored binary
//! shipped by the protoc-bin-vendored crate. That keeps a plain cargo build working on a machine
//! with no protobuf toolchain installed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto");
    if std::env::var("CARGO_FEATURE_GRPC").is_err() {
        return Ok(());
    }
    if std::env::var_os("PROTOC").is_none() {
        match protoc_bin_vendored::protoc_bin_path() {
            Ok(path) => std::env::set_var("PROTOC", path),
            Err(e) => println!("cargo:warning=protoc not found and could not be vendored: {e}"),
        }
    }
    let protos = [
        "../../proto/agentos/v1/common.proto",
        "../../proto/agentos/v1/capability.proto",
        "../../proto/agentos/v1/control.proto",
        "../../proto/agentos/v1/agent.proto",
    ];
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
