use coder_precommit::providers::GrokProvider;

#[test]
fn model_default_is_grok_4_fast() {
    let p = GrokProvider::default();
    assert_eq!(p.model, "grok-4-fast");
}
