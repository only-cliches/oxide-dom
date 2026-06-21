//! `solite init` — scaffold a new project from the embedded templates.

use std::fs;
use std::io;
use std::path::Path;

// Embedded at compile time so the binary is self-contained. `runtime.d.ts` is
// pulled from the canonical type definitions so a scaffolded project always has
// types matching the runtime it links against.
const INDEX_TSX: &str = include_str!("../../../templates/index.tsx");
const STYLES_CSS: &str = include_str!("../../../templates/styles.css");
const TSCONFIG_JSON: &str = include_str!("../../../templates/tsconfig.json");
const BUILD_RS_EXAMPLE: &str = include_str!("../../../templates/build.rs.example");
const MOUNT_RS_EXAMPLE: &str = include_str!("../../../templates/mount.rs.example");
// Vendored copy of the canonical `js/types/solite-runtime.d.ts`, kept fresh by
// this crate's build script. Embedded so a scaffolded project's types match the
// runtime.
const RUNTIME_DTS: &str = include_str!("../../../templates/runtime.d.ts");

const FILES: [(&str, &str); 6] = [
    ("index.tsx", INDEX_TSX),
    ("styles.css", STYLES_CSS),
    ("tsconfig.json", TSCONFIG_JSON),
    ("runtime.d.ts", RUNTIME_DTS),
    ("build.rs.example", BUILD_RS_EXAMPLE),
    ("mount.rs.example", MOUNT_RS_EXAMPLE),
];

pub fn run(dir: &Path, force: bool) -> io::Result<()> {
    if dir.exists() {
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a directory", dir.display()),
            ));
        }
        let non_empty = fs::read_dir(dir)?.next().is_some();
        if non_empty && !force {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} is not empty; pass --force to scaffold into it anyway",
                    dir.display()
                ),
            ));
        }
    } else {
        fs::create_dir_all(dir)?;
    }

    for (name, contents) in FILES {
        fs::write(dir.join(name), contents)?;
    }

    println!("Scaffolded solite project in {}", dir.display());
    for (name, _) in FILES {
        println!("  {name}");
    }
    Ok(())
}
