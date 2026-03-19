#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;

use alloc::vec::Vec;
use alloc::string::String;
use getopts::Options;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: cp <source> <dest>\nCopy a file."));
        return 0;
    }
    if matches.free.len() < 2 {
        println!("cp: missing operand");
        println!("Usage: cp <source> <dest>");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("cp: failed to get current task");
        return -1;
    };

    let src_path: &Path = matches.free[0].as_ref();
    match src_path.get(&cwd) {
        Some(FileOrDir::File(f)) => {
            let mut locked = f.lock();
            let size = locked.len();
            let mut buf = alloc::vec![0u8; size];
            if let Err(e) = locked.read_at(&mut buf, 0) {
                println!("cp: error reading '{}': {:?}", matches.free[0], e);
                return -1;
            }
            // Try to write to dest
            let dest_path: &Path = matches.free[1].as_ref();
            match dest_path.get(&cwd) {
                Some(FileOrDir::File(df)) => {
                    let mut dest_locked = df.lock();
                    match dest_locked.write_at(&buf, 0) {
                        Ok(_) => {
                            println!("cp: copied '{}' to '{}'", matches.free[0], matches.free[1]);
                        }
                        Err(e) => {
                            println!("cp: error writing '{}': {:?}", matches.free[1], e);
                            return -1;
                        }
                    }
                }
                Some(FileOrDir::Dir(_)) => {
                    println!("cp: target '{}' is a directory (copying into dirs not yet supported)", matches.free[1]);
                    return -1;
                }
                None => {
                    println!("cp: cannot create '{}': file creation not yet fully supported on MaiOS", matches.free[1]);
                    println!("cp: read {} bytes from source, but destination does not exist", buf.len());
                    return -1;
                }
            }
        }
        Some(FileOrDir::Dir(_)) => {
            println!("cp: '{}' is a directory (use -r for directories, not yet supported)", matches.free[0]);
            return -1;
        }
        None => {
            println!("cp: cannot open '{}': No such file", matches.free[0]);
            return -1;
        }
    }
    0
}
