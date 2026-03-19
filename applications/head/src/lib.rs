#![no_std]
#[macro_use] extern crate app_io;

#[macro_use] extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;

use core::str;
use alloc::{
    string::String,
    vec::Vec,
    vec,
};
use getopts::Options;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");
    opts.optopt("n", "lines", "number of lines to show (default 10)", "N");

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

    let num_lines: usize = matches.opt_str("n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    if matches.free.is_empty() {
        println!("head: missing file operand");
        return -1;
    }

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("head: failed to get current task");
        return -1;
    };

    for file_arg in &matches.free {
        let path: &Path = file_arg.as_ref();
        match path.get(&cwd) {
            Some(FileOrDir::File(file)) => {
                if matches.free.len() > 1 {
                    println!("==> {} <==", file_arg);
                }
                let mut file_locked = file.lock();
                let file_size = file_locked.len();
                let mut buf = vec![0u8; file_size];
                match file_locked.read_at(&mut buf, 0) {
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&buf);
                        for (i, line) in text.lines().enumerate() {
                            if i >= num_lines { break; }
                            println!("{}", line);
                        }
                    },
                    Err(e) => {
                        println!("head: {}: read error: {:?}", file_arg, e);
                    }
                }
            },
            Some(FileOrDir::Dir(_)) => println!("head: {}: is a directory", file_arg),
            None => println!("head: {}: no such file", file_arg),
        }
    }
    0
}

fn print_usage(opts: Options) {
    println!("{}", opts.usage(USAGE));
}

const USAGE: &str = "Usage: head [-n N] FILE...
Print the first N lines of each FILE (default 10)";
