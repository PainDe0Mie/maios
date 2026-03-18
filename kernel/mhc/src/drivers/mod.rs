//! MHC GPU Drivers — Hardware backend implementations.
//!
//! Each driver implements the `GpuDevice` trait and registers itself
//! with the MHC device registry during initialization.
//!
//! Available drivers:
//! - `software`: CPU-emulated GPU for testing (always available)
//! - `virtio_gpu`: VirtIO-GPU 1.2 driver for QEMU (requires `virtio` feature)

pub mod software;

#[cfg(feature = "virtio")]
pub mod virtio_gpu;
