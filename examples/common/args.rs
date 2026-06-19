use std::path::PathBuf;

/// Parse a `--capture <path>` / `--capture=<path>` CLI flag, with a fallback to
/// the `SOLITE_CAPTURE` env var. When present, examples capture the next
/// painted frame to a PNG at that path and exit.
#[allow(dead_code)]
pub fn capture_path_from_cli() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--capture=") {
            return Some(PathBuf::from(rest));
        }
        if arg == "--capture" || arg == "-c" {
            return args.next().map(PathBuf::from);
        }
    }

    std::env::var_os("SOLITE_CAPTURE").map(PathBuf::from)
}
