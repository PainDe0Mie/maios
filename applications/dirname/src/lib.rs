#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("Usage: dirname NAME");
        return -1;
    }
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: dirname NAME");
            println!("Strip last component from file name.");
            return 0;
        }
    }
    let path = &args[0];
    let dir = match path.rfind('/') {
        Some(0) => "/",
        Some(pos) => &path[..pos],
        None => ".",
    };
    println!("{}", dir);
    0
}
