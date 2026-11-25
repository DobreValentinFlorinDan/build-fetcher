//! GPU support carried over from irelia-companion: the NVIDIA driver's CUDA interface loaded at run time (no install,
//! no link-time dependency), its PTX kernel for weighted similarities, and the GPU log shown in Settings. The
//! learning backends (crates/learn) decide what runs here and check it against the processor.

pub mod cuda;
pub mod log;
