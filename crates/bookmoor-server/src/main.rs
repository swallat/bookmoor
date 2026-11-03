//! bookmoor server entry point.
//!
//! v0.1 will add the HTTP API (`GET /health`, `GET /v1/tree`, `POST /v1/sync`),
//! storage and the adapter runner. For now this only proves the workspace builds.

fn main() {
    println!("bookmoor-server {} (scaffold)", env!("CARGO_PKG_VERSION"));
}
