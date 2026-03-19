#![no_std]
#[macro_use] extern crate app_io;

extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;
extern crate heapfile;
extern crate vfs_node;

use alloc::{
    string::String,
    vec::Vec,
    vec,
};
use getopts::Options;
use path::Path;
use fs_node::{FileOrDir, DirRef};

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");
    opts.optflag("r", "recursive", "copy directories recursively");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(f) => {
            println!("{}", f);
            print_usage(opts);
            return -1;
        }
    };

    if matches.opt_present("h") {
        print_usage(opts);
        return 0;
    }

    if matches.free.len() < 2 {
        println!("cp: missing operand");
        print_usage(opts);
        return -1;
    }

    let recursive = matches.opt_present("r");

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("cp: failed to get current task");
        return -1;
    };

    let src_path: &Path = matches.free[0].as_ref();
    let dst_name = &matches.free[1];

    match src_path.get(&cwd) {
        Some(FileOrDir::File(src_file)) => {
            // Copy file
            let mut src_locked = src_file.lock();
            let file_size = src_locked.len();
            let mut buf = vec![0u8; file_size];
            match src_locked.read_at(&mut buf, 0) {
                Ok(_) => {},
                Err(e) => {
                    println!("cp: failed to read source: {:?}", e);
                    return -1;
                }
            };
            drop(src_locked);

            // Determine destination
            let dst_path: &Path = dst_name.as_ref();
            match dst_path.get(&cwd) {
                Some(FileOrDir::Dir(dst_dir)) => {
                    // Copy into directory with source filename
                    let src_name = src_file.lock().get_name();
                    match heapfile::HeapFile::from_vec(buf, src_name, &dst_dir) {
                        Ok(_) => {},
                        Err(e) => {
                            println!("cp: failed to create file: {}", e);
                            return -1;
                        }
                    }
                },
                Some(FileOrDir::File(_)) => {
                    println!("cp: destination file already exists: {}", dst_name);
                    return -1;
                },
                None => {
                    // Create new file with dst_name in cwd
                    match heapfile::HeapFile::from_vec(buf, dst_name.clone(), &cwd) {
                        Ok(_) => {},
                        Err(e) => {
                            println!("cp: failed to create file: {}", e);
                            return -1;
                        }
                    }
                }
            }
        },
        Some(FileOrDir::Dir(src_dir)) => {
            if !recursive {
                println!("cp: -r not specified; omitting directory '{}'", matches.free[0]);
                return -1;
            }
            let dst_path: &Path = dst_name.as_ref();
            let dst_dir = match dst_path.get(&cwd) {
                Some(FileOrDir::Dir(d)) => d,
                _ => {
                    // Create destination directory
                    match vfs_node::VFSDirectory::create(dst_name.clone(), &cwd) {
                        Ok(d) => d,
                        Err(e) => {
                            println!("cp: failed to create directory: {}", e);
                            return -1;
                        }
                    }
                }
            };
            if let Err(e) = copy_dir_recursive(&src_dir, &dst_dir) {
                println!("cp: error during copy: {}", e);
                return -1;
            }
        },
        None => {
            println!("cp: cannot stat '{}': no such file or directory", matches.free[0]);
            return -1;
        }
    }
    0
}

fn copy_dir_recursive(src: &DirRef, dst: &DirRef) -> Result<(), &'static str> {
    let children = src.lock().list();
    for child_name in children.iter().rev() {
        match src.lock().get(child_name) {
            Some(FileOrDir::File(file_ref)) => {
                let mut locked = file_ref.lock();
                let size = locked.len();
                let mut buf = vec![0u8; size];
                locked.read_at(&mut buf, 0).map_err(|_| "failed to read file")?;
                let name = locked.get_name();
                drop(locked);
                heapfile::HeapFile::from_vec(buf, name, dst)?;
            },
            Some(FileOrDir::Dir(subdir)) => {
                let name = subdir.lock().get_name();
                let new_subdir = vfs_node::VFSDirectory::create(name, dst)?;
                copy_dir_recursive(&subdir, &new_subdir)?;
            },
            None => {},
        }
    }
    Ok(())
}

fn print_usage(opts: Options) {
    println!("{}", opts.usage(USAGE));
}

const USAGE: &str = "Usage: cp [-r] SOURCE DEST
Copy SOURCE to DEST";
