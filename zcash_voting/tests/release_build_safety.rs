//! Behaviour that must not be compiled out of a release build.
//!
//! Ordinary tests cannot cover this: they run with debug assertions enabled, so
//! anything hidden inside a `debug_assert!` looks like it works. The bug this
//! guards against did exactly that — a helper-attempt reservation performed its
//! only state mutation inside `debug_assert!(state.begin(&url)?)`, so release
//! builds recorded an empty `attempting_urls`, reported success, and left a
//! crash mid-POST with no evidence the helper had been contacted. Every test
//! passed.

use std::path::Path;

/// Scans the crate's sources for fallible calls inside `debug_assert!`.
///
/// A `?` inside the macro means a call that can fail, which means a call doing
/// real work rather than inspecting a value — and the macro erases it in
/// release. The check is line-based, so a `debug_assert!` split across lines
/// escapes it; it is a guard against the obvious form, not a proof.
#[test]
fn no_debug_assert_hides_a_fallible_call() {
    let mut offenders = Vec::new();
    visit(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
        &mut |path, number, line| {
            if line.contains("debug_assert") && line.contains('?') {
                offenders.push(format!("{}:{number}: {}", path.display(), line.trim()));
            }
        },
    );

    assert!(
        offenders.is_empty(),
        "these `debug_assert!`s contain a fallible call, which a release build \
         removes along with whatever work it does:\n  {}",
        offenders.join("\n  ")
    );
}

fn visit(directory: &Path, report: &mut impl FnMut(&Path, usize, &str)) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, report);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                for (index, line) in contents.lines().enumerate() {
                    report(&path, index + 1, line);
                }
            }
        }
    }
}
