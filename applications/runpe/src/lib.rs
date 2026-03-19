//! runpe — Load and execute a Windows PE binary from the MaiOS VFS.
//!
//! Usage: runpe <path_to_pe_file>
//!
//! This application reads a PE64 file from the VFS, loads it via pe_loader,
//! resolves its imports, applies base relocations, and jumps to the entry point
//! with the task set to Windows ExecMode for NT syscall routing.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::string::ToString;
use alloc::format;
use app_io::println;
use io::{ByteReader, KnownLength};
use fs_node::FileOrDir;

pub fn main(args: alloc::vec::Vec<alloc::string::String>) -> isize {
    if args.is_empty() {
        println!("Usage: runpe <path_to_pe_file>");
        println!("Load and execute a Windows PE64 binary.");
        return 1;
    }

    let pe_path = &args[0];
    println!("runpe: loading \"{}\"...", pe_path);

    // Resolve file in VFS
    let cwd = match task::with_current_task(|t| t.env.lock().working_dir.clone()) {
        Ok(d) => d,
        Err(_) => {
            println!("runpe: failed to get working directory");
            return -1;
        }
    };

    let p: &path::Path = pe_path.as_ref();
    let file_ref = match p.get(&cwd) {
        Some(FileOrDir::File(f)) => f,
        Some(FileOrDir::Dir(_)) => {
            println!("runpe: \"{}\" is a directory", pe_path);
            return -1;
        }
        None => {
            println!("runpe: \"{}\" not found", pe_path);
            return -1;
        }
    };

    // Read the file
    let file_len = { file_ref.lock().len() };
    if file_len < 2 {
        println!("runpe: file too small");
        return -1;
    }

    let mut pe_data = vec![0u8; file_len];
    {
        let mut locked = file_ref.lock();
        if locked.read_at(&mut pe_data, 0).is_err() {
            println!("runpe: failed to read file");
            return -1;
        }
    }

    // Validate PE magic
    if pe_data[0] != b'M' || pe_data[1] != b'Z' {
        println!("runpe: not a PE file (missing MZ header)");
        return -1;
    }

    if !pe_loader::is_pe(&pe_data) {
        println!("runpe: invalid PE64 format");
        return -1;
    }

    println!("runpe: valid PE64, loading sections...");

    // Load PE into memory
    let loaded = match pe_loader::load(&pe_data) {
        Ok(l) => l,
        Err(e) => {
            println!("runpe: load failed: {}", e);
            return -1;
        }
    };

    println!("runpe: loaded at {:#x}, entry at {:#x}", loaded.image_base, loaded.entry_point.value());

    // Resolve imports
    let stub_page = match pe_loader::resolve_imports(&pe_data, loaded.image_base) {
        Ok(p) => p,
        Err(e) => {
            println!("runpe: import resolution failed: {}", e);
            return -1;
        }
    };

    println!("runpe: imports resolved, spawning PE task...");

    // Spawn a new task for the PE
    let entry = loaded.entry_point.value();
    let task_name = format!("pe_{}", pe_path);

    let task_result = spawn::new_task_builder(move |_: ()| -> isize {
        // Set Windows ExecMode
        let _ = task::with_current_task(|t| {
            t.0.exec_mode.store(2, core::sync::atomic::Ordering::Release);
        });

        // Keep sections and stubs alive
        let _sections = loaded.sections;
        let _stubs = stub_page;

        // Jump to entry point
        let entry_fn: extern "C" fn() -> ! = unsafe {
            core::mem::transmute(entry)
        };
        entry_fn();
    }, ())
    .name(task_name)
    .spawn();

    match task_result {
        Ok(task_ref) => {
            println!("runpe: PE task spawned (id={})", task_ref.id);
            0
        }
        Err(e) => {
            println!("runpe: failed to spawn task: {}", e);
            -1
        }
    }
}
