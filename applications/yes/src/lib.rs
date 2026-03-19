#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    let output = if args.is_empty() {
        String::from("y")
    } else if args[0] == "-h" || args[0] == "--help" {
        println!("Usage: yes [STRING]");
        println!("Repeatedly output STRING or 'y'.");
        return 0;
    } else {
        let mut s = args[0].clone();
        for a in &args[1..] {
            s.push(' ');
            s.push_str(a);
        }
        s
    };
    // Print a limited number of times to avoid infinite loop in a kernel context
    for _ in 0..1000 {
        println!("{}", output);
    }
    0
}
