//! Public surface preserved for back-compat. The implementation split into
//! `reaper_ports` (no redis dep) and `reaper_redis` (adapter impls).
//! See PR for #459 (CA4 of clean-architecture audit).

pub use crate::reaper_ports::*;
pub use crate::reaper_redis::*;
