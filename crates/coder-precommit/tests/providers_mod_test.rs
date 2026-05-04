use coder_precommit::providers::{
    AcVerifierProvider, ClaudeProvider, GeminiProvider, GrokProvider,
};

fn assert_impl<T: AcVerifierProvider>() {}

#[test]
fn providers_implement_ac_verifier_provider_trait() {
    assert_impl::<ClaudeProvider>();
    assert_impl::<GeminiProvider>();
    assert_impl::<GrokProvider>();
}
