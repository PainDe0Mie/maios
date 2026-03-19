#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;
use core::str;

pub fn main(args: Vec<String>) -> isize {
    if args.len() < 2 {
        println!("Usage: tr SET1 SET2");
        println!("Translate characters: replace each character in SET1 with the");
        println!("corresponding character in SET2. Reads from stdin.");
        return if args.is_empty() || args[0] == "-h" || args[0] == "--help" { 0 } else { -1 };
    }

    let set1: Vec<char> = args[0].chars().collect();
    let set2: Vec<char> = args[1].chars().collect();

    let Ok(stdin) = app_io::stdin() else {
        println!("tr: cannot open stdin");
        return -1;
    };
    let mut buf = [0u8; 4096];
    let mut all = Vec::new();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let text = str::from_utf8(&all).unwrap_or("");

    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        let mut replaced = false;
        for (i, &s1) in set1.iter().enumerate() {
            if c == s1 {
                if i < set2.len() {
                    output.push(set2[i]);
                } else if !set2.is_empty() {
                    output.push(set2[set2.len() - 1]);
                }
                replaced = true;
                break;
            }
        }
        if !replaced {
            output.push(c);
        }
    }
    print!("{}", output);
    0
}
