#![no_std]
#[macro_use] extern crate app_io;

extern crate alloc;
extern crate task;
extern crate path;
extern crate fs_node;
extern crate heapfile;

use alloc::{
    string::String,
    vec::Vec,
};
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("Usage: touch FILE...");
        println!("Create empty file(s) if they don't exist");
        return -1;
    }

    if args[0] == "-h" || args[0] == "--help" {
        println!("Usage: touch FILE...");
        println!("Create empty file(s) if they don't exist");
        return 0;
    }

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("touch: failed to get current task");
        return -1;
    };

    for file_name in &args {
        let path: &Path = file_name.as_ref();
        match path.get(&cwd) {
            Some(FileOrDir::File(_)) => {
                // File already exists, nothing to do (real touch would update timestamp)
            },
            Some(FileOrDir::Dir(_)) => {
                println!("touch: '{}' is a directory", file_name);
            },
            None => {
                // Create empty file
                match heapfile::HeapFile::create(file_name.clone(), &cwd) {
                    Ok(_) => {},
                    Err(e) => {
                        println!("touch: cannot create '{}': {}", file_name, e);
                    }
                }
            }
        }
    }
    0
}
