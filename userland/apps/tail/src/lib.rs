#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;

use alloc::vec::Vec;
use alloc::string::String;
use core::str;
use getopts::Options;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optopt("n", "", "number of lines to print", "NUM");
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: tail [-n NUM] <file>\nPrint the last NUM lines of a file (default 10)."));
        return 0;
    }
    let num_lines: usize = match matches.opt_str("n") {
        Some(s) => match s.parse() {
            Ok(n) => n,
            Err(_) => { println!("tail: invalid number '{}'", s); return -1; }
        },
        None => 10,
    };
    if matches.free.is_empty() {
        println!("tail: missing file operand");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("tail: failed to get current task");
        return -1;
    };
    for filename in &matches.free {
        let p: &Path = filename.as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(f)) => {
                let mut locked = f.lock();
                let size = locked.len();
                let mut buf = alloc::vec![0u8; size];
                match locked.read_at(&mut buf, 0) {
                    Ok(_) => {}
                    Err(e) => { println!("tail: error reading '{}': {:?}", filename, e); return -1; }
                }
                let text = match str::from_utf8(&buf) {
                    Ok(s) => s,
                    Err(e) => { println!("tail: '{}' is not valid UTF-8: {}", filename, e); return -1; }
                };
                let lines: Vec<&str> = text.lines().collect();
                let start = if lines.len() > num_lines { lines.len() - num_lines } else { 0 };
                for line in &lines[start..] {
                    println!("{}", line);
                }
            }
            Some(FileOrDir::Dir(_)) => { println!("tail: '{}' is a directory", filename); return -1; }
            None => { println!("tail: cannot open '{}': No such file", filename); return -1; }
        }
    }
    0
}
