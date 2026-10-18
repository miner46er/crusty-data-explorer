//! Guard for the embedded font stack.
//!
//! egui's bundled fonts (Ubuntu-Light, NotoEmoji-Regular, emoji-icon-font)
//! carry no glyph for most symbols, and epaint paints its replacement box
//! (U+25FB) wherever a character is missing. `●`, `▸`, `▾`, `◌`, `→`, `✓`,
//! `▲`, `▼`, and `✕` all shipped that way: hollow boxes in the sidebar, the
//! tab strip, and the grid. The halfwidth and fullwidth forms block
//! (U+FF01..=U+FFEF) is missing for the same reason.
//!
//! This guard renders every non-ASCII character the sources put inside a
//! string or char literal and fails any character that paints exactly like a
//! private-use character, which is what the replacement box produces. A
//! character that renders as the box is a bug the operator sees, so the test
//! compares pixels rather than a font API: epaint's `has_glyphs` reports a
//! false negative for characters that live in the replacement face.
//!
//! Each character is checked against the family that paints it: a literal on
//! a line that names `monospace` must render in the monospace family, every
//! other literal in the proportional family. The family comes from the line
//! that spells the literal, so a builder chain split across lines must keep
//! `monospace` on the literal's own line. `#[cfg(test)]` items are skipped:
//! a fixture glyph in a unit test is data, not chrome. The Control Pictures
//! block (U+2400..=U+243F) is exempt: `edit.rs` uses U+241E as an out-of-band
//! data sentinel that the UI translates before it paints.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

/// A character no font defines, so its rendering is the replacement box.
const MISSING_REFERENCE: char = '\u{E000}';
/// A second undefined character, to prove the reference paints the box.
const MISSING_WITNESS: char = '\u{E001}';
/// The size the glyphs render at. Coverage is per font family, not per size.
const GLYPH_SIZE: f32 = 22.0;
/// Row pitch of the probe frame: the size plus breathing room.
const ROW_PITCH: f32 = 30.0;
/// The Control Pictures block: data sentinels, never painted as chrome.
const CONTROL_PICTURES: std::ops::RangeInclusive<char> = '\u{2400}'..='\u{243F}';

/// The renderer tolerates one harness at a time, the way `tests/frames.rs`
/// serializes its captures.
static RENDER_LOCK: Mutex<()> = Mutex::new(());

fn render_lock() -> MutexGuard<'static, ()> {
    RENDER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The font family a literal paints in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Proportional,
    Monospace,
}

impl Family {
    /// The font a probe frame renders this family with.
    fn font(self) -> egui::FontId {
        match self {
            Family::Proportional => egui::FontId::proportional(GLYPH_SIZE),
            Family::Monospace => egui::FontId::monospace(GLYPH_SIZE),
        }
    }
}

/// One place a character is spelled, and the family that place paints in.
struct Site {
    place: String,
    family: Family,
}

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

/// What a scan pass is reading, so comments and strings never mix.
#[derive(Clone, Copy, PartialEq)]
enum Scan {
    Code,
    LineComment,
    BlockComment(usize),
    String,
    RawString(usize),
}

/// The non-ASCII characters inside string and char literals, in source
/// order, with their 1-based line. Comments are skipped; `\u{..}` escapes in
/// strings and char literals are resolved; raw strings (`r".."`, `r#".."#`)
/// are read. A char literal is recognized as `'x'` or `'\u{..}'`; an escaped
/// form such as `'\n'` is skipped. Items behind `#[cfg(test)]` are skipped
/// whole, braces and contents.
fn literal_glyphs(text: &str) -> Vec<(usize, char)> {
    let mut found = Vec::new();
    let mut scan = Scan::Code;
    let mut chars = text.chars().peekable();
    let mut line = 1;
    // A `#[cfg(test)]` item being skipped: whether its body has opened, and
    // the brace depth inside it.
    let mut skip_item = false;
    let mut skip_body = false;
    let mut skip_depth = 0usize;
    while let Some(c) = chars.next() {
        if c == '\n' {
            line += 1;
        }
        match scan {
            Scan::LineComment => {
                if c == '\n' {
                    scan = Scan::Code;
                }
            }
            Scan::BlockComment(depth) => {
                if c == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    scan = Scan::BlockComment(depth + 1);
                } else if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    scan = if depth == 1 { Scan::Code } else { Scan::BlockComment(depth - 1) };
                }
            }
            Scan::String => match c {
                '\\' => {
                    // A `\u{..}` escape names a character the source never
                    // spells out, so resolve it like a literal one.
                    if let Some(resolved) = escaped(&mut chars)
                        && !resolved.is_ascii()
                        && !skip_item
                    {
                        found.push((line, resolved));
                    }
                }
                '"' => scan = Scan::Code,
                c if !c.is_ascii() => {
                    if !skip_item {
                        found.push((line, c));
                    }
                }
                _ => {}
            },
            Scan::RawString(hashes) => {
                if c == '"' {
                    let closes = (0..hashes).all(|_| chars.next() == Some('#'));
                    if closes {
                        scan = Scan::Code;
                    }
                } else if !c.is_ascii() && !skip_item {
                    found.push((line, c));
                }
            }
            Scan::Code => {
                if c == '/' && chars.peek() == Some(&'/') {
                    chars.next();
                    scan = Scan::LineComment;
                } else if c == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    scan = Scan::BlockComment(1);
                } else if c == '"' {
                    scan = Scan::String;
                } else if c == 'r' {
                    let mut hashes = 0;
                    while chars.peek() == Some(&'#') {
                        chars.next();
                        hashes += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        scan = Scan::RawString(hashes);
                    }
                } else if c == '\'' {
                    // `'x'` and `'\u{..}'` are char literals; an escaped
                    // form such as `'\n'` carries no glyph, and anything
                    // else is a lifetime, which carries none either.
                    let mut probe = chars.clone();
                    let middle = probe.next();
                    if middle == Some('\\') {
                        chars.next();
                        let resolved = escaped(&mut chars);
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            if let Some(resolved) = resolved
                                && !resolved.is_ascii()
                                && !skip_item
                            {
                                found.push((line, resolved));
                            }
                        }
                    } else if middle.is_some() && probe.next() == Some('\'') {
                        chars.next();
                        chars.next();
                        if let Some(middle) = middle
                            && !middle.is_ascii()
                            && !skip_item
                        {
                            found.push((line, middle));
                        }
                    }
                } else if c == '#' && !skip_item && chars.peek() == Some(&'[') {
                    // A `#[cfg(test)]` item is a fixture, not chrome: skip
                    // its body once the braces open. Other attributes stay in
                    // the scan, so a glyph in one is still checked.
                    let mut probe = chars.clone();
                    probe.next();
                    let mut attribute = String::new();
                    let mut closed = false;
                    for ch in probe.by_ref() {
                        if ch == ']' {
                            closed = true;
                            break;
                        }
                        attribute.push(ch);
                    }
                    if closed && attribute.trim() == "cfg(test)" {
                        chars.next();
                        for ch in chars.by_ref() {
                            if ch == ']' {
                                break;
                            }
                        }
                        skip_item = true;
                        skip_body = false;
                        skip_depth = 0;
                    }
                } else if skip_item {
                    if skip_body {
                        if c == '{' {
                            skip_depth += 1;
                        } else if c == '}' {
                            skip_depth -= 1;
                            if skip_depth == 0 {
                                skip_item = false;
                                skip_body = false;
                            }
                        }
                    } else if c == '{' {
                        skip_body = true;
                        skip_depth = 1;
                    } else if c == ';' {
                        skip_item = false;
                    }
                }
            }
        }
    }
    found
}

/// The character a backslash escape names, with the backslash already
/// consumed. `\u{..}` resolves; the single-character escapes have no glyph.
fn escaped(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
    if chars.peek() == Some(&'u') {
        chars.next();
        if chars.peek() == Some(&'{') {
            chars.next();
            let mut hex = String::new();
            for digit in chars.by_ref() {
                if digit == '}' {
                    break;
                }
                hex.push(digit);
            }
            return u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
        }
        None
    } else {
        chars.next();
        None
    }
}

/// Every non-ASCII character the sources put in a literal, mapped to the
/// sites that spell it and the family each site paints in. A literal on a
/// line that names `monospace` paints in the monospace family: the scan
/// reads the line, not the builder that receives the literal.
fn painted_glyphs() -> BTreeMap<char, Vec<Site>> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut glyphs: BTreeMap<char, Vec<Site>> = BTreeMap::new();
    for path in source_files(&manifest.join("src")) {
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        let shown = path.strip_prefix(manifest).unwrap_or(&path).display().to_string();
        for (line, character) in literal_glyphs(&text) {
            let family = match lines.get(line - 1) {
                Some(source_line) if source_line.contains("monospace") => Family::Monospace,
                _ => Family::Proportional,
            };
            glyphs.entry(character).or_default().push(Site {
                place: format!("{shown}:{line}"),
                family,
            });
        }
    }
    glyphs
}

/// The pixels of one rect, cropped from `image` at its rounded edges. The
/// crop must lie inside the image, so a render that lost a row fails loudly
/// instead of comparing two blank crops.
fn crop(image: &image::RgbaImage, rect: egui::Rect) -> Vec<u8> {
    let x = rect.min.x.round() as u32;
    let y = rect.min.y.round() as u32;
    let w = (rect.width().round() as u32).max(1);
    let h = (rect.height().round() as u32).max(1);
    assert!(
        x + w <= image.width() && y + h <= image.height(),
        "crop {rect:?} leaves the {}x{} frame",
        image.width(),
        image.height()
    );
    image::imageops::crop_imm(image, x, y, w, h).to_image().into_raw()
}

/// Render one family's probe frame: every glyph plus the two undefined
/// references, one per row. Returns the pixels of the replacement box and
/// the family's characters that paint exactly like it.
fn missing_in(family: Family, glyphs: &BTreeMap<char, Vec<Site>>) -> (Vec<u8>, BTreeSet<char>) {
    let rows = glyphs.len() + 2;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(240.0, ROW_PITCH * rows as f32))
        .build_ui(|ui| {
            for character in glyphs.keys().copied().chain([MISSING_REFERENCE, MISSING_WITNESS]) {
                ui.label(egui::RichText::new(character).font(family.font()));
            }
        });
    harness.run();
    let image = harness.render().expect("render the frame");
    let reference = crop(&image, harness.get_by_label(&MISSING_REFERENCE.to_string()).rect());
    let witness = crop(&image, harness.get_by_label(&MISSING_WITNESS.to_string()).rect());
    assert_eq!(
        reference, witness,
        "two undefined characters must paint the same replacement glyph; \
         if they differ, this guard's premise is wrong"
    );
    let mut missing = BTreeSet::new();
    for character in glyphs.keys() {
        let rect = harness.get_by_label(&character.to_string()).rect();
        if crop(&image, rect) == reference {
            missing.insert(*character);
        }
    }
    (reference, missing)
}

#[test]
fn every_painted_glyph_renders_in_the_family_that_paints_it() {
    let _guard = render_lock();
    let glyphs = painted_glyphs();
    assert!(!glyphs.is_empty(), "the scan found no glyphs to check");

    let (_prop_reference, prop_missing) = missing_in(Family::Proportional, &glyphs);
    let (_mono_reference, mono_missing) = missing_in(Family::Monospace, &glyphs);

    let mut offenders = Vec::new();
    for (character, sites) in &glyphs {
        if CONTROL_PICTURES.contains(character) {
            continue;
        }
        for site in sites {
            let missing = match site.family {
                Family::Proportional => &prop_missing,
                Family::Monospace => &mono_missing,
            };
            if missing.contains(character) {
                offenders.push(format!(
                    "U+{:04X} {character:?} at {} renders as the replacement box",
                    *character as u32, site.place,
                ));
            }
        }
    }
    println!("checked {} painted glyphs in both families", glyphs.len());
    assert!(
        offenders.is_empty(),
        "the embedded fonts have no glyph for these characters, so they paint \
         as a hollow replacement box. Use a glyph the stack carries (tests/frames.rs \
         shows the painted marks in use) or paint the shape with egui's Painter. \
         Halfwidth and fullwidth forms (U+FF01..=U+FFEF) have no glyph either:\n{}",
        offenders.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::literal_glyphs;

    #[test]
    fn the_scan_reads_literals_and_skips_comments() {
        let source = r#"
            // a comment with ● and →
            /* block ● /* nested → */ still block ● */
            let a = "a ● b";
            let b = '×';
            let c = "→";
        "#;
        let found: Vec<char> = literal_glyphs(source).into_iter().map(|(_, c)| c).collect();
        assert_eq!(found, ['●', '×', '→']);
    }

    #[test]
    fn the_scan_resolves_escapes_and_raw_strings() {
        let source = r##"
            let a = "\u{25CF}";
            let b = r"▾ ▸";
            let c = r#"◌"#;
            let d = "…";
            let lifetime: &'static str = "·";
            let escaped = '\n';
        "##;
        let found: Vec<char> = literal_glyphs(source).into_iter().map(|(_, c)| c).collect();
        assert_eq!(found, ['●', '▾', '▸', '◌', '…', '·']);
    }

    #[test]
    fn the_scan_resolves_escapes_inside_char_literals() {
        let source = r#"
            let a = '\u{00D7}';
            let b = '\u{2026}';
            let c = '●';
            let d = '\n';
            let e: &'static str = "ok";
        "#;
        let found: Vec<char> = literal_glyphs(source).into_iter().map(|(_, c)| c).collect();
        assert_eq!(found, ['×', '…', '●']);
    }

    #[test]
    fn the_scan_skips_cfg_test_items() {
        let source = r#"
            let kept = "●";
            #[cfg(test)]
            mod tests {
                const FIXTURE: char = '×';
                #[test]
                fn one() {
                    let sample = "◌ ▾";
                    let escaped = '\u{2714}';
                }
            }
            let also_kept = "→";
            let other = "\u{25B8}";
        "#;
        let found: Vec<char> = literal_glyphs(source).into_iter().map(|(_, c)| c).collect();
        assert_eq!(found, ['●', '→', '▸']);
    }
}
