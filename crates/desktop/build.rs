use std::path::Path;

fn main() {
    // Tauri embeds the built UI at compile time. Without it, `generate_context!`
    // fails deep inside a proc macro with no hint about what to do, so check
    // first and say plainly what is missing.
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist/index.html");
    if !dist.exists() {
        println!("cargo::error=the web UI has not been built yet");
        println!("cargo::error=run: cd ui && npm install && npm run build");
        return;
    }

    // Rebuild the shell whenever the UI is rebuilt.
    println!("cargo::rerun-if-changed=../../ui/dist");
    tauri_build::build();
}
