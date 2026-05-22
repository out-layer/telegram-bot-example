//! Optional, feature-gated extensions that are **not** part of the core
//! stateless tipbot. Anything here may relax the "zero on-disk state"
//! guarantee and is therefore opt-in via a Cargo feature.

#[cfg(feature = "dice")]
pub mod dice;
