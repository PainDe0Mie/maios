#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate time;

use alloc::vec::Vec;
use alloc::string::String;
use time::Instant;

pub fn main(args: Vec<String>) -> isize {
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: uptime");
            println!("Print how long the system has been running.");
            return 0;
        }
    }
    // Instant::ZERO represents boot time; elapsed since then is uptime
    let uptime = Instant::ZERO.elapsed();
    let total_secs = uptime.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    println!("up {}:{:02}:{:02}", hours, minutes, secs);
    0
}
