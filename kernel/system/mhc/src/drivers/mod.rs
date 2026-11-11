//! MHC GPU Drivers — Hardware backend implementations.
//!
//! Each driver implements the `GpuDevice` trait and registers itself
//! with the MHC device registry during initialization.
//!
//! Available drivers:
//! - `software`: CPU-emulated GPU for testing (always available)
//!
//! Le GPU virtio de QEMU a son propre pilote : `kernel/drivers/gpu/virtio_gpu` (plan 08).

pub mod software;
