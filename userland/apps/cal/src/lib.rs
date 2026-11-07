#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    let mut month: u32 = 3; // default March
    let mut year: u32 = 2026; // default 2026

    let mut i = 0;
    while i < args.len() {
        if args[i] == "-h" || args[i] == "--help" {
            println!("Usage: cal [MONTH [YEAR]]");
            println!("Display a calendar for the given month and year.");
            return 0;
        }
        i += 1;
    }

    if !args.is_empty() {
        month = match args[0].parse() {
            Ok(m) if m >= 1 && m <= 12 => m,
            _ => { println!("cal: invalid month '{}'", args[0]); return -1; }
        };
    }
    if args.len() > 1 {
        year = match args[1].parse() {
            Ok(y) if y >= 1 => y,
            _ => { println!("cal: invalid year '{}'", args[1]); return -1; }
        };
    }

    let month_names = [
        "January", "February", "March", "April", "May", "June",
        "July", "August", "September", "October", "November", "December",
    ];
    let title = alloc::format!("{} {}", month_names[(month - 1) as usize], year);
    // Center the title in 20 chars
    let pad = if title.len() < 20 { (20 - title.len()) / 2 } else { 0 };
    for _ in 0..pad { print!(" "); }
    println!("{}", title);
    println!("Su Mo Tu We Th Fr Sa");

    let days_in_month = days_in(month, year);
    let start_dow = day_of_week(year, month, 1);

    // Print leading spaces
    for _ in 0..start_dow {
        print!("   ");
    }
    for day in 1..=days_in_month {
        if day < 10 {
            print!(" {} ", day);
        } else {
            print!("{} ", day);
        }
        if (start_dow + day as usize) % 7 == 0 {
            println!("");
        }
    }
    if (start_dow + days_in_month as usize) % 7 != 0 {
        println!("");
    }
    0
}

fn is_leap(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in(month: u32, year: u32) -> u32 {
    match month {
        1 => 31, 2 => if is_leap(year) { 29 } else { 28 },
        3 => 31, 4 => 30, 5 => 31, 6 => 30,
        7 => 31, 8 => 31, 9 => 30, 10 => 31, 11 => 30, 12 => 31,
        _ => 30,
    }
}

// Zeller-like: returns 0=Sunday, 1=Monday, ..., 6=Saturday
fn day_of_week(year: u32, month: u32, day: u32) -> usize {
    // Tomohiko Sakamoto's algorithm
    let t = [0u32, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    ((y + y / 4 - y / 100 + y / 400 + t[(month - 1) as usize] + day) % 7) as usize
}
