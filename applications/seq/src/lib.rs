#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("Usage: seq [FIRST [INCREMENT]] LAST");
        return -1;
    }
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: seq [FIRST [INCREMENT]] LAST");
            println!("Print numbers from FIRST to LAST with INCREMENT step.");
            return 0;
        }
    }

    let (first, increment, last) = match args.len() {
        1 => {
            let last: i64 = match args[0].parse() {
                Ok(n) => n,
                Err(_) => { println!("seq: invalid number '{}'", args[0]); return -1; }
            };
            (1i64, 1i64, last)
        }
        2 => {
            let first: i64 = match args[0].parse() {
                Ok(n) => n,
                Err(_) => { println!("seq: invalid number '{}'", args[0]); return -1; }
            };
            let last: i64 = match args[1].parse() {
                Ok(n) => n,
                Err(_) => { println!("seq: invalid number '{}'", args[1]); return -1; }
            };
            (first, 1i64, last)
        }
        _ => {
            let first: i64 = match args[0].parse() {
                Ok(n) => n,
                Err(_) => { println!("seq: invalid number '{}'", args[0]); return -1; }
            };
            let inc: i64 = match args[1].parse() {
                Ok(n) => n,
                Err(_) => { println!("seq: invalid number '{}'", args[1]); return -1; }
            };
            let last: i64 = match args[2].parse() {
                Ok(n) => n,
                Err(_) => { println!("seq: invalid number '{}'", args[2]); return -1; }
            };
            (first, inc, last)
        }
    };

    if increment == 0 {
        println!("seq: increment must not be zero");
        return -1;
    }

    let mut current = first;
    if increment > 0 {
        while current <= last {
            println!("{}", current);
            current += increment;
        }
    } else {
        while current >= last {
            println!("{}", current);
            current += increment;
        }
    }
    0
}
