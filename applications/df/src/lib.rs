#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: df");
            println!("Display disk space usage (stub).");
            return 0;
        }
    }
    println!("Filesystem     Size  Used  Avail  Use%  Mounted on");
    println!("ramfs             -     -      -     -   /");
    println!("df: detailed disk statistics not yet available on MaiOS");
    0
}
