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
    vec,
};
use getopts::Options;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");
    opts.optflag("l", "lines", "print the line counts");
    opts.optflag("w", "words", "print the word counts");
    opts.optflag("c", "bytes", "print the byte counts");

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

    let show_lines = matches.opt_present("l");
    let show_words = matches.opt_present("w");
    let show_bytes = matches.opt_present("c");
    // If no specific flag, show all
    let show_all = !show_lines && !show_words && !show_bytes;

    if matches.free.is_empty() {
        // Read from stdin
        let stdin = match app_io::stdin() {
            Ok(s) => s,
            Err(_) => {
                println!("wc: failed to open stdin");
                return -1;
            }
        };
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let cnt = match stdin.read(&mut buf) {
                Ok(c) => c,
                Err(_) => break,
            };
            if cnt == 0 { break; }
            data.extend_from_slice(&buf[..cnt]);
        }
        let text = String::from_utf8_lossy(&data);
        let (lines, words, bytes) = count_text(&text, data.len());
        print_counts(lines, words, bytes, None, show_lines, show_words, show_bytes, show_all);
        return 0;
    }

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("wc: failed to get current task");
        return -1;
    };

    let mut total_lines = 0usize;
    let mut total_words = 0usize;
    let mut total_bytes = 0usize;

    for file_arg in &matches.free {
        let path: &Path = file_arg.as_ref();
        match path.get(&cwd) {
            Some(FileOrDir::File(file)) => {
                let mut file_locked = file.lock();
                let file_size = file_locked.len();
                let mut buf = vec![0u8; file_size];
                match file_locked.read_at(&mut buf, 0) {
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&buf);
                        let (lines, words, bytes) = count_text(&text, file_size);
                        total_lines += lines;
                        total_words += words;
                        total_bytes += bytes;
                        print_counts(lines, words, bytes, Some(file_arg), show_lines, show_words, show_bytes, show_all);
                    },
                    Err(e) => {
                        println!("wc: {}: read error: {:?}", file_arg, e);
                    }
                }
            },
            Some(FileOrDir::Dir(_)) => println!("wc: {}: is a directory", file_arg),
            None => println!("wc: {}: no such file", file_arg),
        }
    }

    if matches.free.len() > 1 {
        print_counts(total_lines, total_words, total_bytes, Some(&"total".to_string()), show_lines, show_words, show_bytes, show_all);
    }
    0
}

fn count_text(text: &str, byte_count: usize) -> (usize, usize, usize) {
    let lines = text.lines().count();
    let words = text.split_whitespace().count();
    (lines, words, byte_count)
}

fn print_counts(lines: usize, words: usize, bytes: usize, name: Option<&String>, show_l: bool, show_w: bool, show_c: bool, show_all: bool) {
    let mut parts = String::new();
    if show_all || show_l {
        parts.push_str(&format!("{:>8}", lines));
    }
    if show_all || show_w {
        parts.push_str(&format!("{:>8}", words));
    }
    if show_all || show_c {
        parts.push_str(&format!("{:>8}", bytes));
    }
    if let Some(n) = name {
        parts.push_str(&format!(" {}", n));
    }
    println!("{}", parts);
}

fn print_usage(opts: Options) {
    println!("{}", opts.usage(USAGE));
}

const USAGE: &str = "Usage: wc [-lwc] [FILE...]
Print line, word, and byte counts for each FILE";
