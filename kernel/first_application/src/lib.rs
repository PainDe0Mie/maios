//! This crate contains a simple routine to start the first application (or set of applications). 
//! 
//! This should be invoked at or towards the end of the kernel initialization procedure. 
//!
//! ## Important Dependency Note
//!  
//! In general, Mai kernel crates *cannot* depend on application crates.
//! However, this crate is a special exception in that it directly loads and runs
//! the first application crate.
//! 
//! Thus, it's safest to ensure that first application crate is always included
//! in the build by specifying it as a direct dependency here.
//! 
//! Currently, that crate is `applications/shell`, but if it changes,
//! we should change that dependendency in this crates `Cargo.toml` manifest.

#![no_std]

extern crate alloc;
#[macro_use] extern crate log;
extern crate spawn;
extern crate mod_mgmt;
extern crate path;

use alloc::format;
use mod_mgmt::CrateNamespace;

/// See the crate-level docs and this crate's `Cargo.toml` for more.
const FIRST_APPLICATION_CRATE_NAME: &str = {
    #[cfg(all(target_arch = "x86_64", feature = "qemu_test"))] { "qemu_test-" }
    #[cfg(all(target_arch = "x86_64", not(feature = "qemu_test")))] { "shell-" }
    #[cfg(target_arch = "aarch64")] { "hello-" }
};

/// Starts the first applications that run in Mai 
/// by creating a new "default" application namespace
/// and spawning the first application `Task`(s). 
/// 
/// Currently this only spawns a shell (terminal),
/// but in the future it could spawn a fuller desktop environment. 
/// 
/// Kernel initialization routines should be complete before invoking this. 
pub fn start() -> Result<(), &'static str> {
    let new_app_ns = mod_mgmt::create_application_namespace(None)?;

    // NOTE: see crate-level docs and note in this crate's `Cargo.toml`.
    // Prefer an `explorer` application, then `desktop`, otherwise fall back to the configured default.
    let candidates = ["explorer-", "shell-", FIRST_APPLICATION_CRATE_NAME];
    let mut found = None;
    for cand in &candidates {
        if let Some((app_file, _ns)) = CrateNamespace::get_crate_object_file_starting_with(&new_app_ns, cand) {
            found = Some(app_file);
            break;
        }
    }

    let app_file = found.ok_or("Couldn't find first application (desktop or default) in default app namespace")?;
    let path = app_file.lock().get_absolute_path();
    info!("Starting first application: crate at {:?}", path);

    let task_builder = match spawn::new_application_task_builder(path.as_ref(), Some(new_app_ns)) {
        Ok(tb) => {
            info!("first_application: task builder created successfully for {:?}", path);
            tb
        }
        Err(e) => {
            error!("first_application: FAILED to create task builder for {:?}: {}", path, e);
            return Err(e);
        }
    };

    match task_builder.name(format!("first_{}", &path)).spawn() {
        Ok(joinable) => {
            info!("first_application: spawned task successfully");
            Ok(())
        }
        Err(e) => {
            error!("first_application: FAILED to spawn task for {:?}: {}", path, e);
            Err(e)
        }
    }
}
