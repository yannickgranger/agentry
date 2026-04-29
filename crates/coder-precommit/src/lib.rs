//! Pre-commit gates and AC verification binaries used by the coder role.

pub mod ac_verifier;
pub mod git_operator;
pub mod providers;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }
}
