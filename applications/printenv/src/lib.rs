#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;

use alloc::vec::Vec;
use alloc::string::String;

pub fn main(args: Vec<String>) -> isize {
    for arg in &args {
        if arg == "-h" || arg == "--help" {
            println!("Usage: printenv [VARIABLE]...");
            println!("Print environment variables. If no VARIABLE specified, print all.");
            return 0;
        }
    }
    let Ok(result) = task::with_current_task(|t| {
        let env = t.env.lock();
        if args.is_empty() {
            // Print all variables
            let mut vars = Vec::new();
            vars.push(alloc::format!("PWD={}", env.cwd()));
            for (k, v) in env.variables.iter() {
                vars.push(alloc::format!("{}={}", k, v));
            }
            vars.push(String::from("HOSTNAME=maios"));
            vars.push(String::from("SHELL=/shell"));
            vars.push(String::from("OS=MaiOS"));
            vars
        } else {
            // Print specific variables
            let mut vars = Vec::new();
            for name in &args {
                let val = match name.as_str() {
                    "PWD" => Some(env.cwd()),
                    "HOSTNAME" => Some(String::from("maios")),
                    "SHELL" => Some(String::from("/shell")),
                    "OS" => Some(String::from("MaiOS")),
                    other => env.get(other).cloned(),
                };
                if let Some(v) = val {
                    vars.push(v);
                }
            }
            vars
        }
    }) else {
        println!("printenv: failed to get current task");
        return -1;
    };
    for v in &result {
        println!("{}", v);
    }
    0
}
