fn main() {
    println!("cargo:rerun-if-env-changed=ENDO_RELEASE_TAG");
}
