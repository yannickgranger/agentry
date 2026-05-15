//! Unit tests for the pre-commit callers gate (Phase 2.3 of #87).

use agentry_role_runtime::{
    decide_gate, derive_fqn, is_orphan_pub_item, orphan_pub_item_finding, parse_allowlist_toml,
    parse_new_pub_items, GateDecision, NewPubItem, PublicApiAllowlist,
};
use orchestrator_types::{FindingOrigin, Severity};

fn item(file: &str, line: u32, kind: &str, name: &str) -> NewPubItem {
    NewPubItem {
        kind: kind.to_string(),
        bare_name: name.to_string(),
        fqn: derive_fqn(file, name),
        file: file.to_string(),
        line,
    }
}

// ---------- parse_allowlist_toml ----------

#[test]
fn parse_allowlist_toml_empty_list_returns_empty() {
    let raw = "[allowed]\nnames = []\n";
    let (a, warn) = parse_allowlist_toml(raw);
    assert!(a.names.is_empty());
    assert!(warn.is_none());
}

#[test]
fn parse_allowlist_toml_multi_entry_list() {
    let raw = "\
# header comment
[allowed]
names = [
  \"agentry_role_runtime::DoneGuard\",
  \"orchestrator_types::Brief\",
]
";
    let (a, warn) = parse_allowlist_toml(raw);
    assert!(warn.is_none());
    assert_eq!(
        a.names,
        vec![
            "agentry_role_runtime::DoneGuard".to_string(),
            "orchestrator_types::Brief".to_string(),
        ]
    );
    assert!(a.contains("agentry_role_runtime::DoneGuard"));
    assert!(!a.contains("nope::Nope"));
}

#[test]
fn parse_allowlist_toml_inline_array() {
    let raw = "[allowed]\nnames = [\"a::B\", \"c::D\"]\n";
    let (a, warn) = parse_allowlist_toml(raw);
    assert!(warn.is_none());
    assert_eq!(a.names, vec!["a::B".to_string(), "c::D".to_string()]);
}

#[test]
fn parse_allowlist_toml_malformed_returns_empty_with_warn() {
    let cases = [
        // Missing [allowed] table.
        "names = [\"a::b\"]\n",
        // Missing closing bracket.
        "[allowed]\nnames = [\"a::b\"\n",
        // Unterminated string literal inside the names array.
        "[allowed]\nnames = [\"unterminated\n",
    ];
    for raw in cases {
        let (a, warn) = parse_allowlist_toml(raw);
        assert!(
            a.names.is_empty(),
            "malformed input should produce empty allowlist: {raw:?}"
        );
        assert!(
            warn.is_some(),
            "malformed input should produce a warn message: {raw:?}"
        );
    }
}

#[test]
fn parse_allowlist_toml_allowed_table_without_names_is_empty_no_warn() {
    let raw = "[allowed]\n";
    let (a, warn) = parse_allowlist_toml(raw);
    assert!(a.names.is_empty());
    assert!(warn.is_none());
}

// ---------- is_orphan_pub_item ----------

#[test]
fn is_orphan_pub_item_zero_callers_not_allowlisted_is_orphan() {
    let it = item("crates/foo/src/lib.rs", 7, "fn", "bar");
    let allow = PublicApiAllowlist::default();
    assert!(is_orphan_pub_item(&it, 0, &allow));
}

#[test]
fn is_orphan_pub_item_zero_callers_but_allowlisted_is_not_orphan() {
    let it = item("crates/foo/src/lib.rs", 7, "fn", "bar");
    let allow = PublicApiAllowlist {
        names: vec!["foo::bar".into()],
    };
    assert!(!is_orphan_pub_item(&it, 0, &allow));
}

#[test]
fn is_orphan_pub_item_with_callers_is_not_orphan() {
    let it = item("crates/foo/src/lib.rs", 7, "fn", "bar");
    let allow = PublicApiAllowlist::default();
    assert!(!is_orphan_pub_item(&it, 1, &allow));
    assert!(!is_orphan_pub_item(&it, 42, &allow));
}

#[test]
fn orphan_pub_item_finding_carries_blocker_and_anchors() {
    let it = item("crates/foo/src/lib.rs", 12, "fn", "bar");
    let f = orphan_pub_item_finding(&it);
    assert!(matches!(f.severity, Severity::Blocker));
    assert_eq!(f.category, "orphan_pub_item");
    match &f.origin {
        FindingOrigin::Mechanical { tool, rule } => {
            assert_eq!(tool, "pre_commit_callers_gate");
            assert_eq!(rule.as_deref(), Some("orphan_pub_item"));
        }
        _ => panic!("expected mechanical origin"),
    }
    assert_eq!(f.file.as_deref(), Some("crates/foo/src/lib.rs"));
    assert_eq!(f.line, Some(12));
    assert!(f.message.contains("foo::bar"));
    assert!(f.message.contains("crates/foo/src/lib.rs:12"));
    assert!(f.message.contains("zero callers"));
    assert!(f.message.contains("allowlist"));
}

// ---------- decide_gate (skip-on-missing-binary path + decision shape) ----------

#[test]
fn decide_gate_skips_when_ra_query_unavailable() {
    let items = vec![item("crates/foo/src/lib.rs", 1, "fn", "bar")];
    let allow = PublicApiAllowlist::default();
    let decision = decide_gate(false, &items, &allow, |_| {
        panic!("resolver must not be called when ra-query is unavailable")
    });
    assert!(matches!(decision, GateDecision::SkippedNoRaQuery));
}

#[test]
fn decide_gate_skips_on_resolver_error() {
    let items = vec![item("crates/foo/src/lib.rs", 1, "fn", "bar")];
    let allow = PublicApiAllowlist::default();
    let decision = decide_gate(true, &items, &allow, |_| Err("ra-query exit 1".into()));
    match decision {
        GateDecision::SkippedResolverFailed(detail) => {
            assert!(detail.contains("ra-query exit 1"));
        }
        other => panic!("expected SkippedResolverFailed, got {other:?}"),
    }
}

#[test]
fn decide_gate_clean_when_all_have_callers() {
    let items = vec![
        item("crates/foo/src/lib.rs", 1, "fn", "bar"),
        item("crates/foo/src/lib.rs", 5, "struct", "Baz"),
    ];
    let allow = PublicApiAllowlist::default();
    let decision = decide_gate(true, &items, &allow, |_| Ok(3));
    assert!(matches!(decision, GateDecision::Clean));
}

#[test]
fn decide_gate_clean_when_orphan_is_allowlisted() {
    let items = vec![item("crates/foo/src/lib.rs", 1, "fn", "bar")];
    let allow = PublicApiAllowlist {
        names: vec!["foo::bar".into()],
    };
    let decision = decide_gate(true, &items, &allow, |_| Ok(0));
    assert!(matches!(decision, GateDecision::Clean));
}

#[test]
fn decide_gate_fires_blockers_for_orphans() {
    let items = vec![
        item("crates/foo/src/lib.rs", 1, "fn", "orphan_one"),
        item("crates/foo/src/lib.rs", 5, "struct", "OrphanTwo"),
        item("crates/foo/src/lib.rs", 9, "fn", "has_caller"),
    ];
    let allow = PublicApiAllowlist::default();
    let decision = decide_gate(true, &items, &allow, |name| {
        if name == "has_caller" {
            Ok(2)
        } else {
            Ok(0)
        }
    });
    match decision {
        GateDecision::BlockersFired(findings) => {
            assert_eq!(findings.len(), 2);
            assert!(findings[0].message.contains("foo::orphan_one"));
            assert!(findings[1].message.contains("foo::OrphanTwo"));
        }
        other => panic!("expected BlockersFired, got {other:?}"),
    }
}

#[test]
fn decide_gate_clean_when_no_items() {
    let allow = PublicApiAllowlist::default();
    let decision = decide_gate(true, &[], &allow, |_| Ok(0));
    assert!(matches!(decision, GateDecision::Clean));
}

// ---------- parse_new_pub_items / derive_fqn (parser surface for the gate) ----------

#[test]
fn parse_new_pub_items_recognises_mod_and_use() {
    let diff = "\
--- a/crates/foo/src/lib.rs
+++ b/crates/foo/src/lib.rs
@@ -0,0 +1,4 @@
+pub mod inner;
+pub use crate::inner::Thing;
+pub use crate::other::Bar as Baz;
+pub fn quack() {}
";
    let items = parse_new_pub_items(diff);
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].kind, "mod");
    assert_eq!(items[0].bare_name, "inner");
    assert_eq!(items[1].kind, "use");
    assert_eq!(items[1].bare_name, "Thing");
    assert_eq!(items[2].kind, "use");
    assert_eq!(items[2].bare_name, "Baz");
    assert_eq!(items[3].kind, "fn");
    assert_eq!(items[3].bare_name, "quack");
}

#[test]
fn parse_new_pub_items_skips_pub_crate_and_pub_super() {
    let diff = "\
--- a/crates/foo/src/lib.rs
+++ b/crates/foo/src/lib.rs
@@ -0,0 +1,3 @@
+pub(crate) fn hidden() {}
+pub(super) struct AlsoHidden;
+pub fn visible() {}
";
    let items = parse_new_pub_items(diff);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].bare_name, "visible");
}

#[test]
fn derive_fqn_uses_crate_dir_with_underscores() {
    assert_eq!(
        derive_fqn("crates/agentry-role-runtime/src/lib.rs", "DoneGuard"),
        "agentry_role_runtime::DoneGuard"
    );
    assert_eq!(
        derive_fqn("crates/orchestrator-types/src/event.rs", "Brief"),
        "orchestrator_types::Brief"
    );
}

#[test]
fn derive_fqn_falls_back_to_bare_name_outside_crates() {
    assert_eq!(derive_fqn("scripts/foo.rs", "bar"), "bar");
    assert_eq!(derive_fqn("Cargo.toml", "x"), "x");
}
