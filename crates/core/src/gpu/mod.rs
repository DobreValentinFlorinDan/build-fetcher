//! Use GPU (0.13.0; ported in 1.1.0): an evaluation's draft similarities on an NVIDIA graphics card, with the same
//! numbers as the processor's (checked before and while it runs).

pub mod accelerate;
pub mod cuda;
pub mod log;
