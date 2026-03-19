#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;

use alloc::vec::Vec;
use alloc::string::String;
use getopts::Options;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: mv <source> <dest>\nMove or rename a file."));
        return 0;
    }
    if matches.free.len() < 2 {
        println!("mv: missing operand");
        println!("Usage: mv <source> <dest>");
        return -1;
    }
    // mv requires atomic rename or copy+delete; neither is fully available yet
    println!("mv: move/rename not yet supported on MaiOS");
    println!("mv: would move '{}' -> '{}'", matches.free[0], matches.free[1]);
    -1
}
