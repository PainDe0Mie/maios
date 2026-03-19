#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate path;
extern crate fs_node;

use alloc::vec::Vec;
use alloc::string::String;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("touch: missing file operand");
        println!("Usage: touch <file>");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("touch: failed to get current task");
        return -1;
    };
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: touch <file>...");
            println!("Update file access time or create empty file.");
            println!("Note: file creation not yet fully supported on MaiOS.");
            return 0;
        }
    }
    for arg in &args {
        let p: &Path = arg.as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(_)) => {
                println!("touched '{}'", arg);
            }
            Some(FileOrDir::Dir(_)) => {
                println!("touched '{}'", arg);
            }
            None => {
                println!("touch: cannot create '{}': file creation not yet supported on MaiOS", arg);
            }
        }
    }
    0
}
