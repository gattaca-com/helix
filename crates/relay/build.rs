fn main() {
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile(&["src/api/gossiper/proto/gossipclient.proto"], &["src/gossiper/proto"])
        .unwrap();

    println!("cargo:rerun-if-changed=src/database/postgres/migrations");
}
