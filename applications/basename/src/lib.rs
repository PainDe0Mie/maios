#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("Usage: basename NAME [SUFFIX]");
        return -1;
    }
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: basename NAME [SUFFIX]");
            println!("Strip directory and optionally SUFFIX from NAME.");
            return 0;
        }
    }
    let path = &args[0];
    let name = match path.rfind('/') {
        Some(pos) => &path[pos + 1..],
        None => path.as_str(),
    };
    let result = if args.len() > 1 {
        let suffix = &args[1];
        if name.ends_with(suffix.as_str()) && name.len() > suffix.len() {
            &name[..name.len() - suffix.len()]
        } else {
            name
        }
    } else {
        name
    };
    println!("{}", result);
    0
}
