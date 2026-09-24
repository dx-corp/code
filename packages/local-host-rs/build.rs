use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("../../proto");
    let public_proto = proto_dir.join("deixicpublic/v1/sdk.proto");
    println!("cargo:rerun-if-changed={}", public_proto.display());
    let proto_file = proto_dir.join("maestro/v1/headless.proto");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", proto_file.display());

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    env::set_var("PROTOC", protoc);

    let mut config = prost_build::Config::new();
    config.extern_path(".google.protobuf.Value", "::prost_types::Value");
    config.extern_path(".google.protobuf.Struct", "::prost_types::Struct");
    config.extern_path(".google.protobuf.ListValue", "::prost_types::ListValue");
    config.extern_path(".google.protobuf.NullValue", "::prost_types::NullValue");
    config.compile_protos(&[proto_file, public_proto], &[proto_dir])?;

    Ok(())
}
