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

/// Scans for queries that treat `votes.tx_hash` as proof a vote is finished.
///
/// A transaction hash exists only for hash-confirmed submissions: the schema
/// requires `CHECK (confirmation_source != 'tree' OR
/// confirmed_transaction_hash IS NULL)`, so a vote confirmed by an exact-tree
/// scan has none and never will. Six separate defects came from reading it as
/// a completion signal — a deadlocked round, a bundle permanently unable to
/// vote, a stale vote-authority note the chain rejected, a rebuildable on-chain
/// vote, an accepted conflicting intent, and a silently overwritten on-chain
/// vote. The last three failed open, which is the worse direction and the
/// reason this guard exists.
///
/// A test asking whether a vote reached the chain must consider the second
/// witness, `vc_tree_position`, or the durable `chain_submissions` row that
/// proves a POST was released. Either mentioned nearby satisfies the rule: SQL
/// here is written across several lines, so the check reads a window rather
/// than one line. Approximate by construction — it catches the shape those six
/// shared, not every possible spelling.
#[test]
fn no_query_treats_a_transaction_hash_as_the_only_proof_a_vote_finished() {
    /// Lines on either side of the mention that count as the same statement.
    const WINDOW: usize = 12;

    let mut offenders = Vec::new();
    visit_files(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
        &mut |path, contents| {
            let lines: Vec<&str> = contents.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let asks_about_a_vote_hash =
                    line.contains("tx_hash IS NOT NULL") || line.contains("tx_hash IS NULL");
                // Delegation submissions are a separate column with their own
                // lifecycle; this rule is about votes.
                if !asks_about_a_vote_hash || line.contains("delegation_tx_hash") {
                    continue;
                }
                let window = &lines[index.saturating_sub(WINDOW)
                    ..lines.len().min(index + WINDOW + 1)];
                // A compare-and-swap that writes the hash asks whether *this
                // column* already holds a different value, which is a
                // concurrency guard on one write and not a claim about whether
                // the vote finished.
                let guards_its_own_write = line.contains("tx_hash = :tx_hash")
                    && window.iter().any(|nearby| nearby.contains("SET tx_hash"));
                if guards_its_own_write {
                    continue;
                }
                let considers_the_second_witness = window.iter().any(|nearby| {
                    nearby.contains("vc_tree_position") || nearby.contains("chain_submissions")
                });
                if !considers_the_second_witness {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        },
    );

    assert!(
        offenders.is_empty(),
        "these read `votes.tx_hash` without considering `vc_tree_position` or a \
         `chain_submissions` row, so they treat an exact-tree confirmation as \
         unfinished:\n  {}",
        offenders.join("\n  ")
    );
}

fn visit_files(directory: &Path, report: &mut impl FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_files(&path, report);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                report(&path, &contents);
            }
        }
    }
}
