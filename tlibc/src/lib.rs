//! MaiOS libc — a POSIX-compatible C standard library for MaiOS.
//!
//! Provides malloc/free, FILE* I/O, pthreads, time, signals, ctype, dirent,
//! setjmp/longjmp, dlopen/dlsym, and 200+ C standard library functions.
//!
//! Built on top of Theseus kernel APIs (single-address-space, kernel-mode).

#![no_std]
#![feature(ptr_internals)]
#![feature(c_variadic)]
#![feature(core_intrinsics)]
#![feature(linkage)]
#![feature(thread_local)]
#![feature(const_btree_new)]

// Allowances for C-style syntax.
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(unused_unsafe)]

// Needed for "staticlib" crate-type only
extern crate panic_entry;
extern crate heap;

extern crate alloc;
#[macro_use] extern crate log;
extern crate libc; // for C types
extern crate spin;
extern crate memchr;
extern crate cbitset;
extern crate memory;
extern crate task;
extern crate cstr_core;
extern crate core2;
extern crate app_io;
extern crate path;
extern crate root;
extern crate fs_node;
extern crate vfs_node;
extern crate spawn;
extern crate scheduler;
extern crate time;
extern crate sleep;

// Core modules (original)
mod errno;
mod io;
mod globals;
mod stdio;
mod stdlib;
mod string;
mod mm;

// New modules
pub mod ctype;
pub mod file_io;
pub mod unistd;
pub mod time_funcs;
pub mod pthread;
pub mod dirent;
pub mod signal_funcs;
pub mod dlfcn;
pub mod setjmp;


use alloc::vec::Vec;
use cstr_core::CString;


pub use errno::*;
use libc::{c_int, c_char};


/// The entry point that Theseus's task spawning logic will invoke (from its task wrapper).
/// 
/// This function invokes the C language `main()` function after setting up some necessary items:
/// function arguments, environment, stack, etc.
///
/// This must be `no_mangle` because compilers look for the `_start` symbol as a link-time dependency.
#[no_mangle]
pub fn _start(args: &[&str], env: &[&str]) -> c_int {
    warn!("AT TOP OF _start in TLIBC");
    debug!("\n\targs: {:?}\n\tenv:  {:?}", args, env);
    let (_args_cstrings, mut args_char_ptrs) = to_cstring_vec(args);
    let (_env_cstrings,  mut env_char_ptrs)  = to_cstring_vec(env);
    
    // Note: `_args_cstrings` and `_env_cstrings` must persist for all execution of `main()`

    // set the global pointers to the args and the environment
    let args_ptr = args_char_ptrs.as_mut_ptr();
    let env_ptr  = env_char_ptrs .as_mut_ptr();
    unsafe {
        globals::argv          = args_ptr;
        globals::inner_argv    = args_char_ptrs;
        globals::environ       = env_ptr;
        globals::inner_environ = env_char_ptrs;
    }

    let retval: c_int = unsafe {
        main(args.len() as isize, args_ptr, env_ptr)
    };

    debug!("main returned {:?}", retval);

    retval
}


/// Clones the given slice of strings into a `Vec` of `CStrings`,
/// and returns it, along with a `Vec` of C-style strings (`char *`) 
/// that point to the corresponding `CStrings` in the first `Vec`.
fn to_cstring_vec(slice_of_strs: &[&str]) -> (Vec<CString>, Vec<*mut c_char>) {
    let mut cstrings = Vec::with_capacity(slice_of_strs.len()); 
    cstrings.extend(slice_of_strs.iter().filter_map(|&s| CString::new(s).ok()));
    let mut cstr_ptrs = Vec::with_capacity(cstrings.len());
    cstr_ptrs.extend(cstrings.iter().map(|c| c.as_ptr() as *mut _));
    (cstrings, cstr_ptrs)
}


// extern "C" {
//     #[linkage = "weak"]
//     fn main(argc: isize, argv: *mut *mut c_char, envp: *mut *mut c_char) -> c_int;
// }

// The above "extern" block is the right way to do this,
// but this below dummy main fn block allows us to experiment with weird linker commands.
#[linkage = "weak"]
#[no_mangle]
extern "C" fn main(argc: isize, argv: *mut *mut c_char, envp: *mut *mut c_char) -> c_int {
    error!("in dummy main! No main function was provided or linked in.");
    -1
}
