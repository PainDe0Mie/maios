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
        println!("{}", opts.usage("Usage: xxd <file>\nMake a hex dump of a file."));
        return 0;
    }
    if matches.free.is_empty() {
        println!("xxd: missing file operand");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("xxd: failed to get current task");
        return -1;
    };
    let p: &Path = matches.free[0].as_ref();
    match p.get(&cwd) {
        Some(FileOrDir::File(f)) => {
            let mut locked = f.lock();
            let size = locked.len();
            let mut buf = alloc::vec![0u8; size];
            if let Err(e) = locked.read_at(&mut buf, 0) {
                println!("xxd: error reading '{}': {:?}", matches.free[0], e);
                return -1;
            }
            let mut offset = 0usize;
            while offset < buf.len() {
                // Print offset
                print!("{:08x}: ", offset);
                // Print hex bytes (16 per line, grouped by 2)
                let end = core::cmp::min(offset + 16, buf.len());
                for i in offset..offset + 16 {
                    if i < end {
                        print!("{:02x}", buf[i]);
                    } else {
                        print!("  ");
                    }
                    if i % 2 == 1 { print!(" "); }
                }
                // Print ASCII
                print!(" ");
                for i in offset..end {
                    let c = buf[i];
                    if c >= 0x20 && c <= 0x7e {
                        print!("{}", c as char);
                    } else {
                        print!(".");
                    }
                }
                println!("");
                offset += 16;
            }
        }
        Some(FileOrDir::Dir(_)) => {
            println!("xxd: '{}' is a directory", matches.free[0]);
            return -1;
        }
        None => {
            println!("xxd: cannot open '{}': No such file", matches.free[0]);
            return -1;
        }
    }
    0
}
