#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: env");
            println!("Print the current environment variables.");
            return 0;
        }
    }
    let Ok(info) = task::with_current_task(|t| {
        let env = t.env.lock();
        let mut vars = Vec::new();
        vars.push(alloc::format!("PWD={}", env.cwd()));
        for (k, v) in env.variables.iter() {
            vars.push(alloc::format!("{}={}", k, v));
        }
        vars
    }) else {
        println!("env: failed to get current task");
        return -1;
    };
    for var in &info {
        println!("{}", var);
    }
    println!("HOSTNAME=maios");
    println!("SHELL=/shell");
    println!("OS=MaiOS");
    0
}
