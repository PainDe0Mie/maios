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
    opts.optflag("c", "count", "prefix lines by the number of occurrences");
    opts.optflag("d", "repeated", "only print duplicate lines");
    opts.optflag("u", "unique", "only print unique lines");
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: uniq [OPTIONS] [FILE]\nFilter adjacent duplicate lines."));
        return 0;
    }
    let count = matches.opt_present("c");
    let only_dup = matches.opt_present("d");
    let only_uniq = matches.opt_present("u");

    let text = if matches.free.is_empty() {
        let Ok(stdin) = app_io::stdin() else {
            println!("uniq: cannot open stdin");
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
            println!("uniq: failed to get current task");
            return -1;
        };
        let p: &Path = matches.free[0].as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(f)) => {
                let mut locked = f.lock();
                let size = locked.len();
                let mut buf = alloc::vec![0u8; size];
                if locked.read_at(&mut buf, 0).is_err() {
                    println!("uniq: error reading '{}'", matches.free[0]);
                    return -1;
                }
                String::from(str::from_utf8(&buf).unwrap_or(""))
            }
            _ => {
                println!("uniq: cannot open '{}'", matches.free[0]);
                return -1;
            }
        }
    };

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let mut cnt = 1usize;
        while i + cnt < lines.len() && lines[i + cnt] == lines[i] {
            cnt += 1;
        }
        let print_it = if only_dup {
            cnt > 1
        } else if only_uniq {
            cnt == 1
        } else {
            true
        };
        if print_it {
            if count {
                println!("{:>7} {}", cnt, lines[i]);
            } else {
                println!("{}", lines[i]);
            }
        }
        i += cnt;
    }
    0
}
