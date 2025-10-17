fn main() {
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile(&["src/api/gossiper/proto/gossipclient.proto"], &["src/gossiper/proto"])
        .unwrap();
}
