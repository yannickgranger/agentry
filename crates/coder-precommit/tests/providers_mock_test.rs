//! Test-only `MockProvider` lives at the integration-test layer (no
//! `#[cfg(test)]` items in src/). Each tests/*.rs is its own crate, so
//! mocks are inlined per test file that needs them; this file verifies
//! the canned-response contract a mock must satisfy.

use coder_precommit::providers::AcVerifierProvider;

struct MockProvider {
    canned_response: String,
}

impl AcVerifierProvider for MockProvider {
    fn verify(&self, _system: &str, _user: &str) -> std::io::Result<String> {
        Ok(self.canned_response.clone())
    }
}

#[test]
fn mock_provider_returns_canned_response_verbatim() {
    let p = MockProvider {
        canned_response: "hello world".into(),
    };
    let r = p.verify("system", "user").expect("verify ok");
    assert_eq!(r, "hello world");
}

#[test]
fn mock_provider_ignores_inputs() {
    let p = MockProvider {
        canned_response: "fixed".into(),
    };
    let a = p.verify("a", "b").expect("a/b");
    let b = p.verify("c", "d").expect("c/d");
    assert_eq!(a, b);
}
