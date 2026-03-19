#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: free");
            println!("Display memory usage information.");
            return 0;
        }
    }
    println!("             total       used       free");
    println!("Mem:   detailed memory statistics not yet available on MaiOS");
    println!("Swap:  not available (no swap on MaiOS)");
    0
}
