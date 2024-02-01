fn main() {
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile(&["src/gossiper/proto/gossipclient.proto"], &["src/gossiper/proto"])
        .unwrap();
}
