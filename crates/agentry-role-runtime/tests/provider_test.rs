//! Public-surface tests migrated from `src/bin/ac_verifier_runner.rs`'s
//! prior inline `#[cfg(test)] mod tests` block. The `Provider` enum +
//! `parse` + `binary_name` were promoted from bin-internal to `pub` items
//! on `agentry_role_runtime` so this file can drive them as a downstream
//! crate user would. See brief X.7b.

use agentry_role_runtime::Provider;

#[test]
fn provider_parse_known_values() {
    assert_eq!(Provider::parse("claude"), Some(Provider::Claude));
    assert_eq!(Provider::parse("gemini"), Some(Provider::Gemini));
    assert_eq!(Provider::parse("grok"), Some(Provider::Grok));
}

#[test]
fn provider_parse_unknown_returns_none() {
    assert_eq!(Provider::parse("openai"), None);
    assert_eq!(Provider::parse(""), None);
}

#[test]
fn provider_binary_name_matches_bash_command_v_target() {
    // These exact strings appear in the bash `command -v` checks of the
    // three AC_VERIFIER_*_AGENTRY_SCRIPT consts — keeping the Rust port
    // wire-compatible with the host bind-mounts.
    assert_eq!(Provider::Claude.binary_name(), "ac-verifier");
    assert_eq!(Provider::Gemini.binary_name(), "ac-verifier-gemini");
    assert_eq!(Provider::Grok.binary_name(), "ac-verifier-grok");
}
