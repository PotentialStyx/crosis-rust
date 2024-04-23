use prost_build::Config;
extern crate prost_build;

fn main() {
    println!("cargo:rerun-if-changed=src/api.proto");

    // Compile protobufs
    let mut config = Config::new();
    config
        .compile_protos(&["src/api.proto"], &["src/"])
        .unwrap();
}
