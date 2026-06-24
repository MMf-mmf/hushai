//! Contract §7 CI guard: the backend MUST NOT branch its behaviour on
//! `source_kind`. It may be decoded, stored, and logged — never used in a
//! conditional or comparison. This walks the crate's own source and fails if any
//! `source_kind` line also contains control-flow / comparison syntax.

use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_server_logic_branches_on_source_kind() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "found no source files to scan");

    // Tokens that indicate a branch/comparison rather than decode/store/log.
    const FORBIDDEN: &[&str] = &["if ", "match ", "==", "!=", "matches!", "=> "];

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read source file");
        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Skip comments (incl. the proto/doc comments that mention source_kind).
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if !line.contains("source_kind") {
                continue;
            }
            if FORBIDDEN.iter().any(|tok| line.contains(tok)) {
                violations.push(format!("{}:{}: {}", file.display(), lineno + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "source_kind appears in server control flow (violates contract §7):\n{}",
        violations.join("\n")
    );
}
