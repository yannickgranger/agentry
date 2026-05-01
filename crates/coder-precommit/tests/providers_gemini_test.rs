use coder_precommit::providers::GeminiProvider;

#[test]
fn model_default_is_gemini_3_flash_preview() {
    let p = GeminiProvider::default();
    assert_eq!(p.model, "gemini-3-flash-preview");
}
