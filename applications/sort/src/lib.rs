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
    opts.optflag("r", "reverse", "reverse sort order");
    opts.optflag("n", "numeric-sort", "sort numerically");
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: sort [OPTIONS] [FILE]\nSort lines of text."));
        return 0;
    }
    let reverse = matches.opt_present("r");
    let numeric = matches.opt_present("n");

    let text = if matches.free.is_empty() {
        // Read from stdin
        let Ok(stdin) = app_io::stdin() else {
            println!("sort: cannot open stdin");
            return -1;
        };
        let mut buf = [0u8; 4096];
        let mut all = Vec::new();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        String::from(str::from_utf8(&all).unwrap_or(""))
    } else {
        let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
            println!("sort: failed to get current task");
            return -1;
        };
        let p: &Path = matches.free[0].as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(f)) => {
                let mut locked = f.lock();
                let size = locked.len();
                let mut buf = alloc::vec![0u8; size];
                if locked.read_at(&mut buf, 0).is_err() {
                    println!("sort: error reading '{}'", matches.free[0]);
                    return -1;
                }
                String::from(str::from_utf8(&buf).unwrap_or(""))
            }
            Some(FileOrDir::Dir(_)) => {
                println!("sort: '{}' is a directory", matches.free[0]);
                return -1;
            }
            None => {
                println!("sort: cannot open '{}': No such file", matches.free[0]);
                return -1;
            }
        }
    };

    let mut lines: Vec<&str> = text.lines().collect();
    if numeric {
        lines.sort_by(|a, b| {
            let na: i64 = a.trim().parse().unwrap_or(0);
            let nb: i64 = b.trim().parse().unwrap_or(0);
            na.cmp(&nb)
        });
    } else {
        lines.sort();
    }
    if reverse {
        lines.reverse();
    }
    for line in &lines {
        println!("{}", line);
    }
    0
}
