//! Replit api protobuf messages

#![allow(clippy::all)]
// Include the `goval` module, which is generated from api.proto.
include!(concat!(env!("OUT_DIR"), "/api.rs"));
