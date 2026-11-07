#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate path;
extern crate fs_node;

use alloc::vec::Vec;
use alloc::string::String;
use core::str;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: rev [FILE]");
            println!("Reverse each line of input.");
            return 0;
        }
    }

    let text = if args.is_empty() {
        let Ok(stdin) = app_io::stdin() else {
            println!("rev: cannot open stdin");
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
            println!("rev: failed to get current task");
            return -1;
        };
        let p: &Path = args[0].as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(f)) => {
                let mut locked = f.lock();
                let size = locked.len();
                let mut buf = alloc::vec![0u8; size];
                if locked.read_at(&mut buf, 0).is_err() {
                    println!("rev: error reading '{}'", args[0]);
                    return -1;
                }
                String::from(str::from_utf8(&buf).unwrap_or(""))
            }
            _ => {
                println!("rev: cannot open '{}'", args[0]);
                return -1;
            }
        }
    };

    for line in text.lines() {
        let reversed: String = line.chars().rev().collect();
        println!("{}", reversed);
    }
    0
}
