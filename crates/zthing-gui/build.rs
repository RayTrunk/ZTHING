use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    // Reading the $MFT needs administrator rights until the service of milestone 5 indexes.
    let manifest =
        new_manifest("ZTHING").requested_execution_level(ExecutionLevel::RequireAdministrator);
    if let Err(e) = embed_manifest(manifest) {
        println!("cargo:warning=embedding the manifest failed: {e}");
        std::process::exit(1);
    }
    println!("cargo:rerun-if-changed=build.rs");
}
