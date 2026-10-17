//! Guard for the embedded font stack.
//!
//! egui's bundled fonts carry no glyph for the fullwidth and halfwidth
//! forms block (U+FF01..=U+FF60), so any character from that block paints
//! as a hollow box in both themes. The ASCII forms do have glyphs, so the
//! sources stay out of the block.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `*.rs` file under `root`, sorted so a failure reads in a stable
/// order.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("one directory entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn sources_stay_out_of_the_fullwidth_forms_block() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for path in source_files(&manifest.join("src")) {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for (line, text) in text.lines().enumerate() {
            for (column, character) in text.chars().enumerate() {
                if ('\u{FF01}'..='\u{FF60}').contains(&character) {
                    let shown = path.strip_prefix(manifest).unwrap_or(&path);
                    offenders.push(format!(
                        "{}:{}:{}: {character:?} (U+{:04X})",
                        shown.display(),
                        line + 1,
                        column + 1,
                        character as u32,
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the embedded fonts have no glyph for the fullwidth and halfwidth \
         forms block (U+FF01..=U+FF60), so these characters would render as \
         hollow boxes; write the ASCII form instead:\n{}",
        offenders.join("\n")
    );
}
