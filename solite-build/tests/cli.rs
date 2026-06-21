//! End-to-end tests for the `solite-build` CLI (`init` + `bundle`).
//!
//! Only built when the `cli` feature is enabled (which is what provides the
//! `solite-build` binary). Run with: `cargo test -p solite-build --features cli`.
#![cfg(feature = "cli")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn solite() -> Command {
    Command::new(env!("CARGO_BIN_EXE_solite-build"))
}

fn workdir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("solite-cli-{}-{tag}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn init_scaffolds_then_build_roundtrips() {
    let work = workdir("init");
    let proj = work.join("proj");

    // init writes the source files plus host workflow examples.
    let status = solite()
        .args(["init", proj.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "init failed");
    for file in [
        "index.tsx",
        "styles.css",
        "tsconfig.json",
        "runtime.d.ts",
        "build.rs.example",
        "mount.rs.example",
    ] {
        assert!(proj.join(file).is_file(), "init did not write {file}");
    }

    // The scaffolded entry must always compile.
    let entry = proj.join("index.tsx");
    let src = fs::read_to_string(&entry).unwrap();
    solite_build::compile_component_source(&entry, &src).expect("scaffold template must compile");

    // build emits a single .rs of virtual modules.
    let out = work.join("app.rs");
    let status = solite()
        .args(["bundle", proj.to_str().unwrap(), out.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "build failed");

    let generated = fs::read_to_string(&out).unwrap();
    assert!(generated.contains("pub fn modules() -> Vec<solite::VirtualSourceFile>"));
    assert!(generated.contains("pub const ENTRY: &str = \"index.js\";"));
    assert!(generated.contains("\"index.js\".to_string()"));
    assert!(generated.contains("\"styles.css\".to_string()"));
    // The renamed root sentinel survives into the emitted module.
    assert!(generated.contains("__SOL_ROOT__"));
    assert!(!generated.contains("__OX_ROOT__"));

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_walks_graph_and_rewrites_explicit_extensions() {
    let work = workdir("graph");
    let proj = work.join("proj");
    fs::create_dir_all(&proj).unwrap();

    // index.tsx imports a TS helper with an explicit extension + a CSS file.
    fs::write(
        proj.join("index.tsx"),
        "import { render } from \"solite-runtime\";\n\
         import { label } from \"./helper.ts\";\n\
         import \"./styles.css\";\n\
         function App() { return <div>{label}</div>; }\n\
         render(() => App(), __SOL_ROOT__);\n",
    )
    .unwrap();
    fs::write(proj.join("helper.ts"), "export const label = \"hi\";\n").unwrap();
    fs::write(proj.join("styles.css"), ".app { color: red; }\n").unwrap();

    let out = work.join("app.rs");
    let status = solite()
        .args(["bundle", proj.to_str().unwrap(), out.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "build failed");

    let generated = fs::read_to_string(&out).unwrap();
    // All three reachable modules are emitted under .js / .css keys.
    assert!(generated.contains("\"index.js\".to_string()"));
    assert!(generated.contains("\"helper.js\".to_string()"));
    assert!(generated.contains("\"styles.css\".to_string()"));
    // The explicit `./helper.ts` import was rewritten to the emitted `.js` key.
    assert!(generated.contains("\"./helper.js\""));
    assert!(!generated.contains("./helper.ts"));

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_errors_when_no_entry() {
    let work = workdir("noentry");
    // A directory with a stray file but no index/app entrypoint.
    fs::write(work.join("notanentry.txt"), "nope").unwrap();
    let out = work.join("app.rs");

    let output = solite()
        .args(["bundle", work.to_str().unwrap(), out.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "build should fail without an entry"
    );
    assert!(!out.exists(), "no output should be written on failure");

    let _ = fs::remove_dir_all(&work);
}
