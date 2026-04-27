//! Pluggable LLM providers for ac-verifier. Brief 2 ships claude only;
//! briefs 3 (Gemini) and 4 (Grok) add per-file siblings.

pub trait AcVerifierProvider {
    fn verify(&self, system: &str, user: &str) -> std::io::Result<String>;
}

pub mod claude;
pub use claude::ClaudeProvider;

pub mod gemini;
pub use gemini::GeminiProvider;

#[cfg(test)]
pub mod mock;
#[cfg(test)]
pub use mock::MockProvider;
