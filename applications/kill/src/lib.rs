#![no_std]
extern crate alloc;
#[macro_use] extern crate app_io;
// #[macro_use] extern crate debugit;

extern crate task;
extern crate scheduler;
extern crate getopts;

use task::scheduler::remove_task;
use getopts::Options;
use alloc::vec::Vec;
use alloc::string::{String, ToString};

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    
    opts.optflag("h", "help", "print this help menu");
    opts.optflag("r", "reap", 
        "reap the task (consume its exit value) in addition to killing it, removing it from the task list."
    );

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(_f) => {
            println!("{}", _f);
            return -1; 
        }
    };

    if matches.opt_present("h") {
        return print_usage(opts);
    }

    let reap = matches.opt_present("r");

    for task_id_str in matches.free.iter() {
        match task_id_str.parse::<usize>(){
            Ok(task_id) => {
                match kill_task(task_id, reap) {
                    Ok(_) => { }
                    Err(e) => {
                        println!("{}", e);
                        return -1;
                    }
                }
            }, 
            _ => { 
                println!("Invalid argument {}, not a valid task ID (usize)", task_id_str); 
                return -1;
            },
        };   
    }
    0
}


fn kill_task(task_id: usize, reap: bool) -> Result<(), String> {
    use task::{RunState, ExitValue, KillReason};

    let task_ref = task::get_task(task_id)
        .ok_or_else(|| alloc::format!("Task ID {} does not exist", task_id))?;

    let already_exited = matches!(task_ref.runstate.load(), RunState::Exited(_));

    if !already_exited {
        task_ref.runstate.store(RunState::Exited(ExitValue::Killed(KillReason::Requested)));

        remove_task(&task_ref);

        println!("Killed task {}", task_ref.name);
    }

    if reap {
        println!("Task {} handled.", task_id);
        Ok(())
    } else if already_exited {
        Err(alloc::format!("Task {} already exited.", task_id))
    } else {
        Ok(())
    }
}

#[allow(dead_code)]
fn print_usage(opts: Options) -> isize {
    let brief = "Usage: kill [OPTS] TASK_ID".to_string();
    println!("{}", opts.usage(&brief));
    0
}
