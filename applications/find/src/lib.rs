#![no_std]
#[macro_use] extern crate app_io;

extern crate alloc;
extern crate task;
extern crate path;
extern crate fs_node;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use fs_node::{DirRef, FileOrDir};

pub fn main(args: Vec<String>) -> isize {
    if args.is_empty() {
        return find_from_cwd(None);
    }

    // Parse args: find [PATH] [-name PATTERN]
    let mut start_path: Option<String> = None;
    let mut name_pattern: Option<String> = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-name" => {
                if i + 1 < args.len() {
                    name_pattern = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    println!("find: missing argument to -name");
                    return -1;
                }
            },
            "-h" | "--help" => {
                println!("Usage: find [PATH] [-name PATTERN]");
                println!("Search for files in a directory hierarchy");
                return 0;
            },
            _ => {
                if start_path.is_none() {
                    start_path = Some(args[i].clone());
                }
                i += 1;
            }
        }
    }

    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("find: failed to get current task");
        return -1;
    };

    let search_dir = if let Some(ref p) = start_path {
        let path: &path::Path = p.as_ref();
        match path.get(&cwd) {
            Some(FileOrDir::Dir(dir)) => dir,
            Some(FileOrDir::File(_)) => {
                println!("find: {}: not a directory", p);
                return -1;
            },
            None => {
                println!("find: {}: no such directory", p);
                return -1;
            }
        }
    } else {
        cwd
    };

    let prefix = start_path.unwrap_or_else(|| ".".to_string());
    find_recursive(&search_dir, &prefix, &name_pattern);
    0
}

fn find_from_cwd(pattern: Option<String>) -> isize {
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("find: failed to get current task");
        return -1;
    };
    find_recursive(&cwd, ".", &pattern);
    0
}

fn find_recursive(dir: &DirRef, current_path: &str, pattern: &Option<String>) {
    let children = dir.lock().list();
    for child_name in children.iter().rev() {
        let child_path = format!("{}/{}", current_path, child_name);

        let matches = match pattern {
            Some(ref pat) => simple_match(child_name, pat),
            None => true,
        };

        if matches {
            println!("{}", child_path);
        }

        // Recurse into directories
        if let Some(child) = dir.lock().get(child_name) {
            if let FileOrDir::Dir(subdir) = child {
                find_recursive(&subdir, &child_path, pattern);
            }
        }
    }
}

/// Simple glob-style matching: supports * as wildcard
fn simple_match(name: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with('*') && pattern.ends_with('*') {
        let inner = &pattern[1..pattern.len()-1];
        return name.contains(inner);
    }
    if pattern.starts_with('*') {
        let suffix = &pattern[1..];
        return name.ends_with(suffix);
    }
    if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len()-1];
        return name.starts_with(prefix);
    }
    name == pattern
}
