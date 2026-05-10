//! Public surface preserved for back-compat. The implementation split into
//! `lifecycle_ports` (no redis dep) and `lifecycle_redis` (adapter impls).
//! See PR for #459 (CA4 of clean-architecture audit).

pub use crate::lifecycle_ports::*;
pub use crate::lifecycle_redis::*;
