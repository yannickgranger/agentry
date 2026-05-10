use orchestrator_types::{TargetRepo, TargetRepoParseError};
use std::str::FromStr;

#[test]
fn round_trip_display_matches_input() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(format!("{tr}"), "yg/agentry");
}

#[test]
fn bare_input_binds_placeholder_forge() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.forge(), "agency-default");
}

#[test]
fn forge_qualified_parse_captures_prefix() {
    let tr = TargetRepo::from_str("agency:yg/agentry").expect("parse");
    assert_eq!(tr.forge(), "agency");
    assert_eq!(tr.owner(), "yg");
    assert_eq!(tr.repo(), "agentry");
}

#[test]
fn slug_byte_identity_with_legacy_sanitizer() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.slug(), "yg_agentry");
}

#[test]
fn slug_doubles_underscore_in_repo_segment() {
    let tr = TargetRepo::from_str("yg/foo_bar").expect("parse");
    assert_eq!(tr.slug(), "yg_foo__bar");
}

#[test]
fn slug_doubles_underscore_in_owner_segment() {
    let tr = TargetRepo::from_str("yg_foo/bar").expect("parse");
    assert_eq!(tr.slug(), "yg__foo_bar");
}

#[test]
fn slug_resolves_legacy_yg_foo_collision() {
    // Pre-1b: both `yg/foo` and `yg_foo/bar`'s prefix collapsed to "yg_foo".
    // Post-1b: the underscore in `yg_foo` is doubled, so `yg/foo` and
    // `yg_foo/<r>` are distinguishable.
    let a = TargetRepo::from_str("yg/foo").expect("parse a");
    let b = TargetRepo::from_str("yg_foo/bar").expect("parse b");
    assert_eq!(a.slug(), "yg_foo");
    assert_eq!(b.slug(), "yg__foo_bar");
    assert_ne!(a.slug(), b.slug());
}

#[test]
fn clone_url_canonical_format() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(
        tr.clone_url("agency.lab:3000"),
        "https://agency.lab:3000/yg/agentry.git"
    );
}

#[test]
fn rejects_invalid_repo_charset() {
    let err = TargetRepo::from_str("yg/agentry@evil.com").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoInvalidChars);
}

#[test]
fn rejects_total_length_over_limit() {
    let owner = "a".repeat(64);
    let repo = "b".repeat(200);
    let input = format!("{owner}/{repo}");
    assert!(input.len() > 200);
    let err = TargetRepo::from_str(&input).expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::TooLong);
}

#[test]
fn rejects_empty_input() {
    let err = TargetRepo::from_str("").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::Empty);
}

#[test]
fn rejects_missing_slash() {
    let err = TargetRepo::from_str("yg-agentry").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::MissingRepo);
}

#[test]
fn cfdb_keyspace_delegates_to_slug() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.cfdb_keyspace(), tr.slug());
}

#[test]
fn display_qualified_includes_forge() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    assert_eq!(tr.display_qualified(), "agency-default:yg/agentry");
}

#[test]
fn serde_round_trip_through_string() {
    let tr = TargetRepo::from_str("yg/agentry").expect("parse");
    let json = serde_json::to_string(&tr).expect("serialize");
    assert_eq!(json, "\"yg/agentry\"");
    let back: TargetRepo = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, tr);
}

#[test]
fn rejects_owner_starting_with_dot() {
    let err = TargetRepo::from_str(".yg/agentry").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::OwnerStartsWithDotOrDash);
}

#[test]
fn rejects_repo_starting_with_dash() {
    let err = TargetRepo::from_str("yg/-agentry").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoStartsWithDotOrDash);
}

// Brief 1b reviewer BLOCKER: boundary-underscore inputs must be rejected
// at parse time so the slug derivation cannot collapse `(yg_, foo)` and
// `(yg, _foo)` to the same `yg___foo`.

#[test]
fn rejects_owner_trailing_underscore() {
    let err = TargetRepo::from_str("yg_/foo").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::OwnerBoundaryUnderscore);
}

#[test]
fn rejects_repo_leading_underscore() {
    let err = TargetRepo::from_str("yg/_foo").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoBoundaryUnderscore);
}

#[test]
fn rejects_owner_trailing_and_repo_leading_underscore() {
    // Owner is checked first; the error names the owner's failure.
    let err = TargetRepo::from_str("yg_/_foo").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::OwnerBoundaryUnderscore);
}

#[test]
fn rejects_owner_leading_underscore() {
    let err = TargetRepo::from_str("_yg/foo").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::OwnerBoundaryUnderscore);
}

#[test]
fn rejects_repo_trailing_underscore() {
    let err = TargetRepo::from_str("yg/foo_").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoBoundaryUnderscore);
}

#[test]
fn rejects_owner_single_underscore() {
    let err = TargetRepo::from_str("_/foo").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::OwnerBoundaryUnderscore);
}

#[test]
fn rejects_repo_single_underscore() {
    let err = TargetRepo::from_str("yg/_").expect_err("must reject");
    assert_eq!(err, TargetRepoParseError::RepoBoundaryUnderscore);
}

#[test]
fn boundary_underscore_class_no_pair_accepted() {
    // Property: every boundary-underscore permutation in the cartesian
    // product of {bare,leading_,trailing_,both_} × {bare,leading_,trailing_,both_}
    // is rejected, except the (bare, bare) base case. This makes the
    // "no two accepted (owner, repo) pairs collide via boundary
    // underscores" property vacuously true.
    let owner_variants = ["yg", "_yg", "yg_", "_yg_"];
    let repo_variants = ["foo", "_foo", "foo_", "_foo_"];
    for o in &owner_variants {
        for r in &repo_variants {
            let input = format!("{o}/{r}");
            let parsed = TargetRepo::from_str(&input);
            let bare = !o.starts_with('_') && !o.ends_with('_');
            let bare_repo = !r.starts_with('_') && !r.ends_with('_');
            if bare && bare_repo {
                let tr =
                    parsed.unwrap_or_else(|e| panic!("input {input} should parse but got {e:?}"));
                assert_eq!(tr.owner(), *o);
                assert_eq!(tr.repo(), *r);
            } else {
                assert!(
                    parsed.is_err(),
                    "input {input} must be rejected as boundary-underscore variant",
                );
            }
        }
    }
}

#[test]
fn slug_distinct_for_internal_underscore_variants() {
    // Sanity check that ACCEPTED inputs with internal underscores still
    // produce distinct slugs across owner/repo placement.
    let cases = [
        ("yg/foo", "yg_foo"),
        ("yg/foo_bar", "yg_foo__bar"),
        ("yg_foo/bar", "yg__foo_bar"),
        ("y_g/f_o", "y__g_f__o"),
        ("yg/foo__bar", "yg_foo____bar"),
    ];
    let slugs: Vec<String> = cases
        .iter()
        .map(|(input, _)| {
            TargetRepo::from_str(input)
                .unwrap_or_else(|e| panic!("input {input} should parse but got {e:?}"))
                .slug()
        })
        .collect();
    for (i, (input, expected)) in cases.iter().enumerate() {
        assert_eq!(&slugs[i], expected, "slug for {input}");
    }
    // No two slugs collide.
    for i in 0..slugs.len() {
        for j in (i + 1)..slugs.len() {
            assert_ne!(
                slugs[i], slugs[j],
                "slug collision between cases[{i}]={} and cases[{j}]={}",
                cases[i].0, cases[j].0,
            );
        }
    }
}
