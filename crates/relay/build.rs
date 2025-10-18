use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let proto_file = manifest_dir.join("src/gossip/proto/gossipclient.proto");
    let include_dir = manifest_dir.join("src/gossip/proto");

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .out_dir(manifest_dir.join("src/gossip/generated"))
        .compile(&[proto_file], &[include_dir])
        .unwrap();

    println!("cargo:rerun-if-changed=src/database/postgres/migrations");
}
