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
    opts.optflag("l", "lines", "print line count");
    opts.optflag("w", "words", "print word count");
    opts.optflag("c", "bytes", "print byte count");
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: wc [-lwc] <file>...\nCount lines, words, and bytes."));
        return 0;
    }
    let show_lines = matches.opt_present("l");
    let show_words = matches.opt_present("w");
    let show_bytes = matches.opt_present("c");
    let show_all = !show_lines && !show_words && !show_bytes;

    if matches.free.is_empty() {
        println!("wc: missing file operand");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("wc: failed to get current task");
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
                    Err(e) => { println!("wc: error reading '{}': {:?}", filename, e); return -1; }
                }
                let text = str::from_utf8(&buf).unwrap_or("");
                let lines = text.lines().count();
                let words = text.split_whitespace().count();
                let bytes = buf.len();
                let mut output = String::new();
                if show_all || show_lines {
                    output.push_str(&alloc::format!("  {} ", lines));
                }
                if show_all || show_words {
                    output.push_str(&alloc::format!("  {} ", words));
                }
                if show_all || show_bytes {
                    output.push_str(&alloc::format!("  {} ", bytes));
                }
                output.push_str(filename);
                println!("{}", output);
            }
            Some(FileOrDir::Dir(_)) => { println!("wc: '{}' is a directory", filename); return -1; }
            None => { println!("wc: cannot open '{}': No such file", filename); return -1; }
        }
    }
    0
}
