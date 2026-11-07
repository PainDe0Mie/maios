#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("Usage: factor NUMBER...");
        return -1;
    }
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: factor NUMBER...");
            println!("Print the prime factors of each NUMBER.");
            return 0;
        }
    }
    for arg in &args {
        let n: u64 = match arg.parse() {
            Ok(v) => v,
            Err(_) => { println!("factor: '{}' is not a valid number", arg); return -1; }
        };
        print!("{}:", n);
        if n <= 1 {
            println!("");
            continue;
        }
        let mut remaining = n;
        let mut divisor = 2u64;
        while divisor * divisor <= remaining {
            while remaining % divisor == 0 {
                print!(" {}", divisor);
                remaining /= divisor;
            }
            divisor += 1;
        }
        if remaining > 1 {
            print!(" {}", remaining);
        }
        println!("");
    }
    0
}
