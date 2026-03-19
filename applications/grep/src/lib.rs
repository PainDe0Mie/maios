#![no_std]
#[macro_use] extern crate app_io;

#[macro_use] extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use getopts::Options;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");
    opts.optflag("i", "ignore-case", "ignore case distinctions");
    opts.optflag("n", "line-number", "prefix each line with line number");
    opts.optflag("c", "count", "only print a count of matching lines");
    opts.optflag("v", "invert-match", "select non-matching lines");

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

    if matches.free.is_empty() {
        println!("grep: missing PATTERN");
        print_usage(opts);
        return -1;
    }

    let pattern = matches.free[0].clone();
    let ignore_case = matches.opt_present("i");
    let show_line_num = matches.opt_present("n");
    let count_only = matches.opt_present("c");
    let invert = matches.opt_present("v");

    let pattern_lower = if ignore_case {
        to_lowercase(&pattern)
    } else {
        pattern.clone()
    };

    // If no files specified, read from stdin
    if matches.free.len() < 2 {
        let stdin = match app_io::stdin() {
            Ok(s) => s,
            Err(_) => {
                println!("grep: failed to open stdin");
                return -1;
            }
        };
        let mut buf = [0u8; 4096];
        let mut data = Vec::new();
        loop {
            let cnt = match stdin.read(&mut buf) {
                Ok(c) => c,
                Err(_) => break,
            };
            if cnt == 0 { break; }
            data.extend_from_slice(&buf[..cnt]);
        }
        let text = String::from_utf8_lossy(&data);
        grep_text(&text, &pattern_lower, ignore_case, show_line_num, count_only, invert, None);
        return 0;
    }

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("grep: failed to get current task");
        return -1;
    };

    let multiple_files = matches.free.len() > 2;

    for file_arg in &matches.free[1..] {
        let path: &Path = file_arg.as_ref();
        match path.get(&cwd) {
            Some(FileOrDir::File(file)) => {
                let mut file_locked = file.lock();
                let file_size = file_locked.len();
                let mut buf = vec![0u8; file_size];
                match file_locked.read_at(&mut buf, 0) {
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&buf);
                        let prefix = if multiple_files { Some(file_arg.as_str()) } else { None };
                        grep_text(&text, &pattern_lower, ignore_case, show_line_num, count_only, invert, prefix);
                    },
                    Err(e) => println!("grep: {}: read error: {:?}", file_arg, e),
                }
            },
            Some(FileOrDir::Dir(_)) => println!("grep: {}: is a directory", file_arg),
            None => println!("grep: {}: no such file", file_arg),
        }
    }
    0
}

fn grep_text(text: &str, pattern: &str, ignore_case: bool, show_line_num: bool, count_only: bool, invert: bool, prefix: Option<&str>) {
    let mut match_count = 0;

    for (line_num, line) in text.lines().enumerate() {
        let line_to_check = if ignore_case {
            to_lowercase(line)
        } else {
            line.to_string()
        };

        let matches = line_to_check.contains(pattern);
        let should_print = if invert { !matches } else { matches };

        if should_print {
            match_count += 1;
            if !count_only {
                let mut output = String::new();
                if let Some(p) = prefix {
                    output.push_str(p);
                    output.push(':');
                }
                if show_line_num {
                    output.push_str(&format!("{}:", line_num + 1));
                }
                output.push_str(line);
                println!("{}", output);
            }
        }
    }

    if count_only {
        if let Some(p) = prefix {
            println!("{}:{}", p, match_count);
        } else {
            println!("{}", match_count);
        }
    }
}

fn to_lowercase(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        for lc in c.to_lowercase() {
            result.push(lc);
        }
    }
    result
}

fn print_usage(opts: Options) {
    println!("{}", opts.usage(USAGE));
}

const USAGE: &str = "Usage: grep [OPTIONS] PATTERN [FILE...]
Search for PATTERN in each FILE or stdin";
