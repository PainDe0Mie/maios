#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;
extern crate task;
extern crate getopts;
extern crate path;
extern crate fs_node;

use alloc::vec::Vec;
use alloc::string::String;
use getopts::Options;
use path::Path;
use fs_node::FileOrDir;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => { println!("{}", e); return -1; }
    };
    if matches.opt_present("h") {
        println!("{}", opts.usage("Usage: du [PATH]...\nEstimate file space usage."));
        return 0;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("du: failed to get current task");
        return -1;
    };
    let paths = if matches.free.is_empty() {
        alloc::vec![String::from(".")]
    } else {
        matches.free.clone()
    };
    for p_str in &paths {
        let p: &Path = p_str.as_ref();
        match p.get(&cwd) {
            Some(FileOrDir::File(f)) => {
                let locked = f.lock();
                println!("{}\t{}", locked.len(), p_str);
            }
            Some(FileOrDir::Dir(d)) => {
                let locked = d.lock();
                let entries = locked.list();
                let mut total: usize = 0;
                for entry in &entries {
                    if let Some(child) = locked.get(entry) {
                        match child {
                            FileOrDir::File(cf) => {
                                let sz = cf.lock().len();
                                total += sz;
                            }
                            FileOrDir::Dir(_) => {
                                // Would need recursion for deep sizes
                            }
                        }
                    }
                }
                println!("{}\t{}", total, p_str);
            }
            None => {
                println!("du: cannot access '{}': No such file or directory", p_str);
                return -1;
            }
        }
    }
    0
}
