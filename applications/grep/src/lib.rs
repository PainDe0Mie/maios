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
    opts.optflag("i", "ignore-case", "case-insensitive matching");
    opts.optflag("n", "line-number", "print line numbers");
    opts.optflag("c", "count", "print only match count");
    opts.optflag("v", "invert-match", "invert match");
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: grep [OPTIONS] PATTERN [FILE]...\nSearch for PATTERN in each FILE (substring match)."));
        return 0;
    }
    if matches.free.is_empty() {
        println!("grep: missing pattern");
        return -1;
    }
    let pattern = &matches.free[0];
    let ignore_case = matches.opt_present("i");
    let show_numbers = matches.opt_present("n");
    let count_only = matches.opt_present("c");
    let invert = matches.opt_present("v");

    let pattern_lower = if ignore_case {
        to_lowercase(pattern)
    } else {
        pattern.clone()
    };

    if matches.free.len() < 2 {
        // Read from stdin
        let Ok(stdin) = app_io::stdin() else {
            println!("grep: cannot open stdin");
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
        let text = str::from_utf8(&all).unwrap_or("");
        let mut count = 0usize;
        for (i, line) in text.lines().enumerate() {
            let hay = if ignore_case { to_lowercase(&String::from(line)) } else { String::from(line) };
            let matched = hay.contains(pattern_lower.as_str());
            let matched = if invert { !matched } else { matched };
            if matched {
                count += 1;
                if !count_only {
                    if show_numbers {
                        println!("{}:{}", i + 1, line);
                    } else {
                        println!("{}", line);
                    }
                }
            }
        }
        if count_only { println!("{}", count); }
        if count > 0 { 0 } else { 1 }
    } else {
        let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
            println!("grep: failed to get current task");
            return -1;
        };
        let multi = matches.free.len() > 2;
        let mut any_match = false;
        for filename in &matches.free[1..] {
            let p: &Path = filename.as_ref();
            match p.get(&cwd) {
                Some(FileOrDir::File(f)) => {
                    let mut locked = f.lock();
                    let size = locked.len();
                    let mut buf = alloc::vec![0u8; size];
                    if locked.read_at(&mut buf, 0).is_err() {
                        println!("grep: error reading '{}'", filename);
                        continue;
                    }
                    let text = match str::from_utf8(&buf) {
                        Ok(s) => s,
                        Err(_) => { println!("grep: '{}' is not valid UTF-8", filename); continue; }
                    };
                    let mut count = 0usize;
                    for (i, line) in text.lines().enumerate() {
                        let hay = if ignore_case { to_lowercase(&String::from(line)) } else { String::from(line) };
                        let matched = hay.contains(pattern_lower.as_str());
                        let matched = if invert { !matched } else { matched };
                        if matched {
                            count += 1;
                            any_match = true;
                            if !count_only {
                                let prefix = if multi { alloc::format!("{}:", filename) } else { String::new() };
                                if show_numbers {
                                    println!("{}{}:{}", prefix, i + 1, line);
                                } else {
                                    println!("{}{}", prefix, line);
                                }
                            }
                        }
                    }
                    if count_only {
                        if multi { println!("{}:{}", filename, count); }
                        else { println!("{}", count); }
                    }
                }
                Some(FileOrDir::Dir(_)) => { println!("grep: '{}': Is a directory", filename); }
                None => { println!("grep: '{}': No such file", filename); }
            }
        }
        if any_match { 0 } else { 1 }
    }
}

fn to_lowercase(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        for lc in c.to_lowercase() {
            out.push(lc);
        }
    }
    out
}
