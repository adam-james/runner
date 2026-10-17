//! Builds the python tool: CPython plus `app.py`, as a component exporting
//! `tool-api`, written next to the other components in
//! `target/wasm32-wasip2/release/`. Run with
//! `cargo run -p build-python --release`.

use std::env;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = here.join("../..");
    // Like cargo, a relative CARGO_TARGET_DIR is relative to where it's run.
    let target = env::var_os("CARGO_TARGET_DIR")
        .map(|dir| env::current_dir().unwrap_or_default().join(dir))
        .unwrap_or_else(|| root.join("target"));
    let out = target.join("wasm32-wasip2/release/python.wasm");
    std::fs::create_dir_all(out.parent().unwrap())?;

    componentize_py::command::run([
        "componentize-py".into(),
        "-d".into(),
        root.join("wit").into_os_string(),
        "-w".into(),
        "tool-guest".into(),
        "componentize".into(),
        "app".into(),
        "-p".into(),
        here.into_os_string(),
        "-o".into(),
        out.clone().into_os_string(),
    ])?;
    println!("built {}", out.display());
    Ok(())
}
