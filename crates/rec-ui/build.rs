fn main() {
    // `generate_context!` embeds `ui/dist`, which only exists after
    // `npm run build`. A placeholder keeps `cargo build --workspace` (and CI,
    // which has no Node) working; the app then says how to build the UI.
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(&dist).expect("create ui/dist");
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><meta charset=utf-8><title>rec</title>\
             <body style=\"font:14px system-ui;background:#16181d;color:#ddd;padding:2em\">\
             The UI isn't built. Run <code>npm install</code> and <code>npm run build</code> \
             in <code>ui/</code>, then rebuild rec-ui.",
        )
        .expect("write placeholder ui/dist/index.html");
    }
    tauri_build::build();
}
