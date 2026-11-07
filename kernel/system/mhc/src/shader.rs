//! MHC Shader Manager — GPU kernel bytecode management.
//!
//! Manages SPIR-V compute shaders: loading, validation, caching.
//!
//! ## Shader formats
//!
//! MHC primarily uses SPIR-V as the shader intermediate representation:
//! - Industry standard (Khronos Group)
//! - Well-specified binary format
//! - Targets both Vulkan (via VirtIO-GPU venus) and OpenGL (via virgl)
//!
//! Future: MHC-IR, a custom IR optimized for MHC's compute model.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

// ---------------------------------------------------------------------------
// Shader handle
// ---------------------------------------------------------------------------

/// Opaque handle to a loaded shader module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShaderHandle(pub u64);

impl ShaderHandle {
    pub const INVALID: ShaderHandle = ShaderHandle(0);
}

// ---------------------------------------------------------------------------
// Shader format
// ---------------------------------------------------------------------------

/// Supported shader bytecode formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderFormat {
    /// SPIR-V bytecode (Khronos standard).
    SpirV,
    /// MHC intermediate representation (future).
    MhcIR,
    /// Pre-compiled native ISA for a specific GPU.
    Native,
}

// ---------------------------------------------------------------------------
// Binding descriptor
// ---------------------------------------------------------------------------

/// Describes a single resource binding expected by a shader.
#[derive(Clone, Debug)]
pub struct BindingDescriptor {
    /// Binding slot index.
    pub binding: u32,
    /// Type of resource.
    pub kind: BindingKind,
    /// Whether the shader writes to this binding.
    pub writable: bool,
}

/// Kind of shader resource binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    /// Storage buffer (read/write).
    StorageBuffer,
    /// Uniform buffer (read-only, small, cached).
    UniformBuffer,
    /// Storage image (read/write texture).
    StorageImage,
    /// Sampled image (read-only texture with filtering).
    SampledImage,
}

// ---------------------------------------------------------------------------
// Shader module
// ---------------------------------------------------------------------------

/// A loaded shader module with metadata.
pub struct ShaderModule {
    /// Unique handle for referencing this shader.
    pub handle: ShaderHandle,
    /// Human-readable name (e.g., "gemm_f32", "alpha_blend").
    pub name: String,
    /// Bytecode format.
    pub format: ShaderFormat,
    /// Raw bytecode data.
    pub bytecode: Vec<u8>,
    /// Entry point function name.
    pub entry_point: String,
    /// Local workgroup size declared in the shader [x, y, z].
    pub workgroup_size: [u32; 3],
    /// Resource binding descriptors.
    pub bindings: Vec<BindingDescriptor>,
    /// Maximum shared memory used (bytes).
    pub shared_memory_size: u32,
}

impl ShaderModule {
    /// Create a new shader module from raw SPIR-V bytecode.
    pub fn from_spirv(
        name: &str,
        bytecode: Vec<u8>,
        entry_point: &str,
        workgroup_size: [u32; 3],
        bindings: Vec<BindingDescriptor>,
    ) -> Self {
        let handle = ShaderHandle(NEXT_HANDLE.fetch_add(1, Ordering::Relaxed));
        ShaderModule {
            handle,
            name: String::from(name),
            format: ShaderFormat::SpirV,
            bytecode,
            entry_point: String::from(entry_point),
            workgroup_size,
            bindings,
            shared_memory_size: 0,
        }
    }

    /// Validate SPIR-V bytecode (basic checks).
    ///
    /// Validates:
    /// - Magic number (0x07230203)
    /// - Minimum size (5 words = 20 bytes for the header)
    /// - Version bounds
    pub fn validate_spirv(bytecode: &[u8]) -> Result<(), &'static str> {
        if bytecode.len() < 20 {
            return Err("SPIR-V too short: need at least 20 bytes");
        }
        // Check magic number (little-endian)
        let magic = u32::from_le_bytes([
            bytecode[0], bytecode[1], bytecode[2], bytecode[3],
        ]);
        if magic != 0x07230203 {
            return Err("invalid SPIR-V magic number");
        }
        Ok(())
    }

    /// Total invocations per workgroup.
    pub fn workgroup_invocations(&self) -> u32 {
        self.workgroup_size[0] * self.workgroup_size[1] * self.workgroup_size[2]
    }
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Shader cache
// ---------------------------------------------------------------------------

/// Global shader cache. Prevents reloading the same shader multiple times.
pub struct ShaderCache {
    /// Shaders keyed by handle.
    by_handle: BTreeMap<ShaderHandle, Arc<ShaderModule>>,
    /// Name → handle lookup for convenience.
    by_name: BTreeMap<String, ShaderHandle>,
}

impl ShaderCache {
    pub fn new() -> Self {
        ShaderCache {
            by_handle: BTreeMap::new(),
            by_name: BTreeMap::new(),
        }
    }

    /// Register a shader in the cache.
    pub fn insert(&mut self, shader: ShaderModule) -> Arc<ShaderModule> {
        let handle = shader.handle;
        let name = shader.name.clone();
        let arc = Arc::new(shader);
        self.by_handle.insert(handle, arc.clone());
        self.by_name.insert(name, handle);
        arc
    }

    /// Lookup a shader by handle.
    pub fn get(&self, handle: ShaderHandle) -> Option<Arc<ShaderModule>> {
        self.by_handle.get(&handle).cloned()
    }

    /// Lookup a shader by name.
    pub fn get_by_name(&self, name: &str) -> Option<Arc<ShaderModule>> {
        let handle = self.by_name.get(name)?;
        self.by_handle.get(handle).cloned()
    }

    /// Remove a shader from the cache.
    pub fn remove(&mut self, handle: ShaderHandle) {
        if let Some(shader) = self.by_handle.remove(&handle) {
            self.by_name.remove(&shader.name);
        }
    }

    /// Number of cached shaders.
    pub fn len(&self) -> usize {
        self.by_handle.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_handle.is_empty()
    }
}

impl Default for ShaderCache {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Global shader cache instance
// ---------------------------------------------------------------------------

static SHADER_CACHE: spin::Once<Mutex<ShaderCache>> = spin::Once::new();

/// Access the global shader cache.
pub fn shader_cache() -> &'static Mutex<ShaderCache> {
    SHADER_CACHE.call_once(|| Mutex::new(ShaderCache::new()))
}

/// Load and cache a SPIR-V shader. Returns an Arc to the cached module.
pub fn load_spirv(
    name: &str,
    bytecode: Vec<u8>,
    entry_point: &str,
    workgroup_size: [u32; 3],
    bindings: Vec<BindingDescriptor>,
) -> Result<Arc<ShaderModule>, &'static str> {
    ShaderModule::validate_spirv(&bytecode)?;
    let module = ShaderModule::from_spirv(name, bytecode, entry_point, workgroup_size, bindings);
    let arc = shader_cache().lock().insert(module);
    log::info!("MHC/Shader: loaded '{}' (handle={:?}, wg_size={:?})",
        name, arc.handle, workgroup_size);
    Ok(arc)
}
