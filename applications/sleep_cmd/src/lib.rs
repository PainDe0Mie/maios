#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate sleep;

use alloc::vec::Vec;
use alloc::string::String;
use core::time::Duration;

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        println!("Usage: sleep_cmd <seconds>");
        return -1;
    }
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: sleep_cmd <seconds>");
            println!("Pause for the specified number of seconds.");
            return 0;
        }
    }
    let secs: u64 = match args[0].parse() {
        Ok(n) => n,
        Err(_) => {
            println!("sleep_cmd: invalid number '{}'", args[0]);
            return -1;
        }
    };
    sleep::sleep(Duration::from_secs(secs)).ok();
    0
}
