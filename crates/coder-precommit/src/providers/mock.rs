//! Test-only provider that returns a canned response regardless of inputs.

use std::io;

use super::AcVerifierProvider;

#[cfg(test)]
pub struct MockProvider {
    pub canned_response: String,
}

#[cfg(test)]
impl AcVerifierProvider for MockProvider {
    fn verify(&self, _system: &str, _user: &str) -> io::Result<String> {
        Ok(self.canned_response.clone())
    }
}
