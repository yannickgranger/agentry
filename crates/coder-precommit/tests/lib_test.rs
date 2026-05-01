use coder_precommit::version;

#[test]
fn version_is_non_empty() {
    assert!(!version().is_empty());
}
