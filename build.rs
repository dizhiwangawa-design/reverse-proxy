fn main() {
    // The frontend is embedded with rust-embed; make sure Cargo recompiles
    // after `npm run build` updates web/dist.
    println!("cargo:rerun-if-changed=web/dist");
}
