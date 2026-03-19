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
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: tee [FILE]\nRead from stdin and write to stdout and FILE."));
        return 0;
    }

    // Read all stdin
    let Ok(stdin) = app_io::stdin() else {
        println!("tee: cannot open stdin");
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

    // Write to stdout
    let text = str::from_utf8(&all).unwrap_or("");
    print!("{}", text);

    // Write to file if specified
    if !matches.free.is_empty() {
        let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
            println!("tee: failed to get current task");
            return -1;
        };
        let p: &Path = matches.free[0].as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(f)) => {
                let mut locked = f.lock();
                match locked.write_at(&all, 0) {
                    Ok(_) => {}
                    Err(e) => {
                        println!("tee: error writing '{}': {:?}", matches.free[0], e);
                        return -1;
                    }
                }
            }
            Some(FileOrDir::Dir(_)) => {
                println!("tee: '{}' is a directory", matches.free[0]);
                return -1;
            }
            None => {
                println!("tee: cannot create '{}': file creation not yet supported on MaiOS", matches.free[0]);
            }
        }
    }
    0
}
