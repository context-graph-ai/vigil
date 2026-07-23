//! Command-line secret-flag inventory for every production crate's `src/`:
//! `crates/vigil/src`, `crates/vigil-ha/src`, and `crates/vigil-bin/src`.
//!
//! A value passed on argv (`vigil run --foo secret-value`) sits in the
//! process list for any other local user to read with `ps`. Exactly one
//! such flag exists today (`--rtsp-password`); removing it is a separate,
//! later product change. This guard's job is narrower: make sure no SECOND
//! secret-shaped flag can appear anywhere in the source — in the parser,
//! the usage string, or a docs table row — without deliberate review.
//! `vigil-ha` and `vigil-bin` carry zero CLI flags of their own today (the
//! adapter has no CLI surface; the composition root's binary parses
//! whatever `vigil::run_cli_with_site_channel` parses), so their share of
//! the baseline starts and stays empty until a reviewed addition earns an
//! entry — scanning them from day one closes the blind spot before either
//! crate grows its own flag parsing.
//!
//! A flag counts as secret-shaped when its name contains one of a small,
//! defensible set of substrings that mean "this value must not be
//! observable on the command line": `password`, `passwd`, `secret`,
//! `token`, `api-key`, `apikey`. These are the common spellings for
//! credential-shaped CLI flags in Rust/Unix tooling; the list is
//! deliberately conservative (it would rather miss an oddly-named secret
//! than flag an unrelated flag like `--fabric-ticket` or `--service-user`).
//!
//! Each site is identified by (source file, flag name) — never a line
//! number, so it survives an unrelated edit to the same file. The scanner
//! is a pure function over source text (`secret_cli_flag_sites`) shared by
//! the real scan below and its own planted-fault proof. Comment/string-
//! literal lexing itself lives in `source_scan_lexer.rs`, shared with
//! `environment_read_surface.rs` so the two guards cannot silently drift
//! apart on that shared concern.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{PRODUCTION_CRATES, crates_root, rust_sources, string_literals};

/// Substrings that mark a CLI flag name as carrying a secret. Deliberately
/// small and literal (no stemming/fuzzing) so the guard's behavior stays
/// obvious from reading this list.
const SECRET_FLAG_INDICATORS: [&str; 6] =
    ["password", "passwd", "secret", "token", "api-key", "apikey"];

/// One command-line flag whose name looks like it carries a secret.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CliFlagSite {
    file: String,
    flag: String,
}

/// A CLI-flag-shaped token, e.g. `--rtsp-password`, found anywhere inside a
/// string literal (a match-arm literal, a `[--flag ARG]` usage-string
/// entry, or a `` `--flag ARG` `` docs-table cell all render the same way).
fn flag_tokens(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut flags = Vec::new();
    let mut i = 0usize;
    while i + 1 < n {
        if chars[i] == '-'
            && chars[i + 1] == '-'
            && chars.get(i + 2).is_some_and(char::is_ascii_lowercase)
        {
            let start = i + 2;
            let mut j = start;
            while j < n
                && (chars[j].is_ascii_lowercase() || chars[j].is_ascii_digit() || chars[j] == '-')
            {
                j += 1;
            }
            flags.push(format!("--{}", chars[start..j].iter().collect::<String>()));
            i = j;
            continue;
        }
        i += 1;
    }
    flags
}

fn is_secret_flag(flag: &str) -> bool {
    let lower = flag.to_ascii_lowercase();
    SECRET_FLAG_INDICATORS
        .iter()
        .any(|needle| lower.contains(needle))
}

/// The pure detector: every secret-shaped CLI flag name mentioned anywhere
/// in `source`'s string literals, labeled with the file identity the
/// caller supplies. Both the real scan and the planted-fault proof below
/// call this one function.
fn secret_cli_flag_sites(file_label: &str, source: &str) -> BTreeSet<CliFlagSite> {
    let mut sites = BTreeSet::new();
    for literal in string_literals(source) {
        for flag in flag_tokens(&literal) {
            if is_secret_flag(&flag) {
                sites.insert(CliFlagSite {
                    file: file_label.to_string(),
                    flag,
                });
            }
        }
    }
    sites
}

// ---------------------------------------------------------------------
// Baseline loading + the real scan.
// ---------------------------------------------------------------------

/// This crate's own manifest directory (`crates/vigil`), used only to
/// locate this file's own baseline text file — the crate-scope source walk
/// itself (`PRODUCTION_CRATES`, `crates_root`, `rust_sources`) is shared
/// with `environment_read_surface.rs` via `source_scan_lexer.rs`, so the
/// two scans cannot silently drift onto different crate lists.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Frozen starting count, so the baseline file cannot be padded with
/// entries that never existed in source just to make room for a new one.
const BASELINE_STARTING_SITE_COUNT: usize = 1;

/// The membership of `cli_secret_flag_surface.baseline.txt` as originally
/// recorded when this subset guard was added — frozen HERE, in this
/// immutable test file. Mirrors `ORIGINAL_ENVIRONMENT_READ_SITES` in
/// `environment_read_surface.rs` and exists for the identical reason: the
/// one-directional check in `no_new_secret_cli_flags_and_baseline_only_shrinks`
/// (every OBSERVED flag must already be in the baseline) does not catch a
/// coordinated swap — remove the one baselined flag from source and the
/// baseline together, add a DIFFERENT secret-shaped flag to both — which
/// keeps `observed` a subset of `baseline` and the count at the ceiling of
/// 1, so it self-approves. The subset check below closes that.
const ORIGINAL_CLI_SECRET_FLAG_SITES: &[(&str, &str)] = &[("vigil/config.rs", "--rtsp-password")];

fn original_secret_cli_flag_sites() -> BTreeSet<CliFlagSite> {
    let sites: BTreeSet<CliFlagSite> = ORIGINAL_CLI_SECRET_FLAG_SITES
        .iter()
        .map(|(file, flag)| CliFlagSite {
            file: (*file).to_string(),
            flag: (*flag).to_string(),
        })
        .collect();
    assert_eq!(
        sites.len(),
        ORIGINAL_CLI_SECRET_FLAG_SITES.len(),
        "ORIGINAL_CLI_SECRET_FLAG_SITES must not contain a duplicate (file, flag) pair"
    );
    sites
}

fn load_baseline() -> BTreeSet<CliFlagSite> {
    let path = crate_root()
        .join("tests")
        .join("cli_secret_flag_surface.baseline.txt");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    parse_baseline(&text)
}

fn parse_baseline(text: &str) -> BTreeSet<CliFlagSite> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.splitn(2, '|');
            let file = fields.next().unwrap_or_default().to_string();
            let flag = fields.next().unwrap_or_default().to_string();
            CliFlagSite { file, flag }
        })
        .collect()
}

fn scan_real_tree() -> BTreeSet<CliFlagSite> {
    let crates_root = crates_root();
    let mut observed = BTreeSet::new();
    for crate_name in PRODUCTION_CRATES {
        let src_root = crates_root.join(crate_name).join("src");
        assert!(
            src_root.is_dir(),
            "expected {} to exist; a guard that cannot find the source tree must fail loudly, \
             not silently scan nothing",
            src_root.display()
        );
        let sources = rust_sources(&src_root);
        assert!(
            !sources.is_empty(),
            "found zero .rs files under {}; a guard that cannot find the source tree must fail \
             loudly, not silently scan nothing",
            src_root.display()
        );
        for path in sources {
            let relative = path
                .strip_prefix(&src_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let label = format!("{crate_name}/{relative}");
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            observed.extend(secret_cli_flag_sites(&label, &source));
        }
    }
    observed
}

#[test]
fn no_new_secret_cli_flags_and_baseline_only_shrinks() {
    let baseline = load_baseline();
    assert!(
        baseline.len() <= BASELINE_STARTING_SITE_COUNT,
        "cli_secret_flag_surface.baseline.txt now lists {} flag(s), above the frozen starting \
         ceiling of {BASELINE_STARTING_SITE_COUNT}; the baseline may only shrink as flags are \
         removed, never grow to make room for a new one",
        baseline.len()
    );

    // A POSITIVE CONTROL, not a "baseline is non-empty" check and not a
    // check that the one real `--rtsp-password` flag is still present
    // (both existed here before and both are removed): the sole baselined
    // flag is exactly the flag this guard's own module doc says removing
    // is a legitimate, separate product change — a guard that fails when
    // that change lands would forbid the very thing it exists to allow.
    // Neither an emptiness check nor a check for one specific real flag
    // can tell "the last secret flag was removed" (success) apart from
    // "the scanner silently broke" (failure); both look like nothing
    // found. This fixture, independent of any real flag, proves the
    // scanner itself still recognizes a secret-shaped flag when one
    // exists.
    let positive_control_sample = r#"
        fn usage() -> &'static str {
            "Usage: vigil run [--test-estate-positive-control-secret VALUE]"
        }
    "#;
    let positive_control_sites =
        secret_cli_flag_sites("positive-control.rs", positive_control_sample);
    assert_eq!(
        positive_control_sites.len(),
        1,
        "positive control: the scanner must always recognize this fixture secret-shaped flag, \
         independent of whether any real flag remains — if this fails, the scanner itself is \
         broken, not merely burned down: {positive_control_sites:#?}"
    );

    let observed = scan_real_tree();
    // A BIDIRECTIONAL check, not a one-way `observed - baseline` check:
    // matches `environment_read_surface.rs`'s
    // `no_new_environment_read_sites_and_baseline_only_shrinks`, and for the
    // identical reason. A one-way check alone lets a STALE baseline entry
    // sit undetected after the real flag is removed from source, and a
    // stale entry is silently permissive: if the same flag is later
    // re-added to source, `observed - baseline` is empty again (the
    // baseline still lists it), so the guard stays green while a secret
    // flag returns to argv — visible in the process list, which is this
    // guard's entire reason for existing. A stale baseline entry must fail
    // exactly as loudly as a new site the baseline never listed, so both
    // directions are asserted together.
    let new_flags: BTreeSet<&CliFlagSite> = observed.difference(&baseline).collect();
    let stale_baseline_entries: BTreeSet<&CliFlagSite> = baseline.difference(&observed).collect();
    assert!(
        new_flags.is_empty() && stale_baseline_entries.is_empty(),
        "cli_secret_flag_surface.baseline.txt and the real scan disagree.\n\
         flag(s) found by the scan but missing from the baseline (add a deliberate, reviewed \
         entry, or remove the flag from source instead): {new_flags:#?}\n\
         flag(s) listed in the baseline but never observed by the scan (either the flag was \
         removed from source — update the baseline to match — or the scan can no longer see it, \
         which is a resolver bug to fix, not an entry to delete): {stale_baseline_entries:#?}\n\
         a value passed on argv is visible in the process list; if a NEW flag is a deliberate, \
         reviewed addition (not a mistake), add it to cli_secret_flag_surface.baseline.txt"
    );

    // Canary for the bidirectional check itself, on synthetic sets — no
    // real file or real source touched: a STALE baseline entry (present in
    // the baseline, absent from the observed scan — the shape burn-down
    // produces when a flag is removed from source but the matching
    // baseline line is left behind) is caught by `baseline - observed`
    // even though `observed - baseline` alone is empty and would have
    // stayed silent under the old one-way check.
    let stale_only_baseline: BTreeSet<CliFlagSite> = BTreeSet::from([CliFlagSite {
        file: "vigil/config.rs".to_string(),
        flag: "--rtsp-password".to_string(),
    }]);
    let stale_only_observed: BTreeSet<CliFlagSite> = BTreeSet::new();
    let stale_canary: BTreeSet<&CliFlagSite> = stale_only_baseline
        .difference(&stale_only_observed)
        .collect();
    assert_eq!(
        stale_canary.len(),
        1,
        "a baseline entry with no matching observed site must be caught by the bidirectional \
         check: got {stale_canary:#?}"
    );
    assert!(
        stale_only_observed
            .difference(&stale_only_baseline)
            .next()
            .is_none(),
        "sanity: the one-way observed-minus-baseline direction alone must stay silent on this \
         exact shape, which is precisely why it used to pass under the old one-way check"
    );

    // A SUBSET check against the frozen original membership — see
    // `ORIGINAL_CLI_SECRET_FLAG_SITES`'s own doc comment for why the
    // one-directional check above cannot catch a coordinated swap.
    let original = original_secret_cli_flag_sites();
    let smuggled: Vec<&CliFlagSite> = baseline.difference(&original).collect();
    assert!(
        smuggled.is_empty(),
        "cli_secret_flag_surface.baseline.txt lists flag(s) that were never part of the original \
         frozen membership ORIGINAL_CLI_SECRET_FLAG_SITES locks in, however the count and the \
         observed-subset-of-baseline check balance out — a hidden secret-on-argv flag cannot \
         enter through this baseline: {smuggled:#?}"
    );
}

// ---------------------------------------------------------------------
// Planted-fault proof: the detector function itself, exercised directly
// (no real files touched), on a sample that must be flagged and a sample
// built only from allowed shapes that must not be.
// ---------------------------------------------------------------------

#[test]
fn detector_flags_a_planted_secret_flag_and_leaves_allowed_shapes_alone() {
    let violation_sample = r#"
        fn parse(arg: &str) {
            match arg {
                "--admin-password" => {}
                "--upstream-api-key" => {}
                _ => {}
            }
        }

        fn usage() -> String {
            "Usage: vigil run [--config PATH] [--admin-password PASSWORD]".to_string()
        }
    "#;
    let sites = secret_cli_flag_sites("violation.rs", violation_sample);
    let flags: BTreeSet<String> = sites.into_iter().map(|site| site.flag).collect();
    assert_eq!(
        flags,
        BTreeSet::from([
            "--admin-password".to_string(),
            "--upstream-api-key".to_string(),
        ]),
        "the detector must flag every secret-shaped flag mention, in a match arm and in a usage \
         string alike"
    );

    let allowed_sample = r#"
        // An old idea was --admin-password; never shipped, do not resurrect it.
        fn parse(arg: &str) {
            match arg {
                "--rtsp-username" => {}
                "--fabric-ticket" => {}
                "--service-user" => {}
                _ => {}
            }
        }
    "#;
    let allowed_sites = secret_cli_flag_sites("allowed.rs", allowed_sample);
    assert!(
        allowed_sites.is_empty(),
        "a commented-out mention and non-secret flag names must not be flagged: \
         {allowed_sites:#?}"
    );
}

#[test]
fn baseline_parses_into_the_expected_count() {
    let baseline = load_baseline();
    // A CEILING, not an equality pin: see the identical reasoning in
    // `environment_read_surface.rs`'s copy of this test — an equality pin
    // would block the terminal burned-down-to-empty state (removing the
    // sole `--rtsp-password` flag) that the subset guard below exists to
    // allow.
    assert!(
        baseline.len() <= BASELINE_STARTING_SITE_COUNT,
        "the checked-in baseline now lists {} flag(s), above the frozen starting ceiling of \
         BASELINE_STARTING_SITE_COUNT ({BASELINE_STARTING_SITE_COUNT}); the baseline may only \
         shrink as flags are removed — including all the way to empty — never grow to make room \
         for a new one, and never re-pinned to the starting value",
        baseline.len()
    );

    // Canary for the subset guard against `ORIGINAL_CLI_SECRET_FLAG_SITES`,
    // exercised directly on parsed baseline text — no real files touched.
    // Both outcomes matter: a coordinated swap must go RED, a legitimate
    // burn-down must stay GREEN.
    let original = original_secret_cli_flag_sites();

    // RED: the sole original entry replaced by a different one — the
    // count stays at the ceiling of 1, the shape a hidden knob would
    // actually take.
    let swapped = parse_baseline("vigil/config.rs|--upstream-secret-key\n");
    assert_eq!(
        swapped.len(),
        BASELINE_STARTING_SITE_COUNT,
        "sanity: the planted swap must keep the same total count a coordinated edit would"
    );
    let smuggled: Vec<&CliFlagSite> = swapped.difference(&original).collect();
    assert_eq!(
        smuggled.len(),
        1,
        "a coordinated swap (the original flag replaced by a different one, count unchanged) \
         must be caught by the subset check even though the count matches: {smuggled:#?}"
    );
    assert_eq!(
        smuggled[0].flag, "--upstream-secret-key",
        "got {smuggled:#?}"
    );

    // GREEN, the TERMINAL case: the baseline burned all the way down to
    // EMPTY — the sole original flag removed, nothing added, the
    // successful end state (the product change this guard's own module
    // doc names as separate and legitimate). The empty set is trivially a
    // subset of anything, so the subset check must stay silent; the
    // positive control (independent of baseline content entirely) must
    // still catch a real flag, proving this is a genuinely clean terminal
    // state rather than a broken scanner that would also find nothing.
    let burned_down = parse_baseline("");
    assert!(
        burned_down.is_empty(),
        "sanity: the planted burn-down must remove the one site"
    );
    let burned_down_smuggled: Vec<&CliFlagSite> = burned_down.difference(&original).collect();
    assert!(
        burned_down_smuggled.is_empty(),
        "a legitimate burn-down must stay a subset of the original membership and pass cleanly: \
         {burned_down_smuggled:#?}"
    );
    let terminal_positive_control_sites = secret_cli_flag_sites(
        "positive-control.rs",
        r#"fn usage() -> &'static str { "Usage: vigil run [--test-estate-positive-control-secret VALUE]" }"#,
    );
    assert_eq!(
        terminal_positive_control_sites.len(),
        1,
        "the positive control must still catch a real secret-shaped flag against a fully \
         burned-down (empty) baseline — proving an empty baseline is a genuinely clean terminal \
         state, not a broken scanner: {terminal_positive_control_sites:#?}"
    );
}
