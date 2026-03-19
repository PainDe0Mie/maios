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
    vec,
};
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() || args.len() < 2 {
        println!("Usage: mv SOURCE DEST");
        println!("Move or rename SOURCE to DEST");
        return -1;
    }

    if args[0] == "-h" || args[0] == "--help" {
        println!("Usage: mv SOURCE DEST");
        println!("Move or rename SOURCE to DEST");
        return 0;
    }

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("mv: failed to get current task");
        return -1;
    };

    let src_path: &Path = args[0].as_ref();
    let dst_name = &args[1];

    match src_path.get(&cwd) {
        Some(FileOrDir::File(src_file)) => {
            // Read source file
            let mut src_locked = src_file.lock();
            let file_size = src_locked.len();
            let mut buf = vec![0u8; file_size];
            match src_locked.read_at(&mut buf, 0) {
                Ok(_) => {},
                Err(e) => {
                    println!("mv: failed to read source: {:?}", e);
                    return -1;
                }
            }
            let src_name = src_locked.get_name();
            drop(src_locked);

            // Determine destination
            let dst_path: &Path = dst_name.as_ref();
            let (dest_dir, dest_name) = match dst_path.get(&cwd) {
                Some(FileOrDir::Dir(dst_dir)) => (dst_dir, src_name),
                _ => (cwd.clone(), dst_name.clone()),
            };

            // Create new file at destination
            match heapfile::HeapFile::from_vec(buf, dest_name, &dest_dir) {
                Ok(_) => {},
                Err(e) => {
                    println!("mv: failed to create destination: {}", e);
                    return -1;
                }
            }

            // Remove source
            let parent = match src_file.lock().get_parent_dir() {
                Some(p) => p,
                None => {
                    println!("mv: cannot determine parent of source");
                    return -1;
                }
            };
            if parent.lock().remove(&FileOrDir::File(src_file.clone())).is_none() {
                println!("mv: failed to remove source");
                return -1;
            }
        },
        Some(FileOrDir::Dir(_)) => {
            println!("mv: moving directories is not yet supported");
            return -1;
        },
        None => {
            println!("mv: cannot stat '{}': no such file or directory", args[0]);
            return -1;
        }
    }
    0
}
