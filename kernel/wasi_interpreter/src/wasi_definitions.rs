//! WASI system call, signature, and permission definitions as well as mappings.
//!
//! This module contains the following:
//! * Macros for easily defining wasmi function signatures.
//! * SystemCall enum type consisting of supported system calls.
//! * Mapping from system call string to SystemCall type.
//! * Mapping between system call number and SystemCall type.
//! * Mapping from SystemCall type to wasmi signature.
//! * Definitions of WASI rights for full file and directory permissions.
//!
//! Signature macro from tomaka/redshirt:
//! <https://github.com/tomaka/redshirt/blob/4df506f68821353a7fd67bb94c4223df6b683e1b/kernel/core/src/primitives.rs>
//!

use alloc::vec::Vec;
use core::convert::TryFrom;
use core::str::FromStr;
use wasmi::{Signature, ValueType};

/// Generates wasmi function signature.
pub fn get_signature(
    params: impl Iterator<Item = ValueType>,
    ret_ty: impl Into<Option<ValueType>>,
) -> Signature {
    wasmi::Signature::new(
        params.map(wasmi::ValueType::from).collect::<Vec<_>>(),
        ret_ty.into().map(wasmi::ValueType::from),
    )
}

/// Macro to efficiently generate wasmi function signature.
#[macro_export]
macro_rules! sig {
    (($($p:ident),*)) => {{
        let params = core::iter::empty();
        $(let params = params.chain(core::iter::once(ValueType::$p));)*
        $crate::wasi_definitions::get_signature(params, None)
    }};
    (($($p:ident),*) -> $ret:ident) => {{
        let params = core::iter::empty();
        $(let params = params.chain(core::iter::once(ValueType::$p));)*
        $crate::wasi_definitions::get_signature(params, Some($crate::ValueType::$ret))
    }};
}

/// WASI system calls that are currently supported.
#[derive(Copy, Clone, Debug)]
pub enum SystemCall {
    // ── Originaux ──────────────────────────────────────────────
    ProcExit,           //  0 — exit le processus
    FdClose,            //  1 — ferme un fd
    FdWrite,            //  2 — écrit dans un fd (writev)
    FdSeek,             //  3 — déplace le curseur (lseek)
    FdRead,             //  4 — lit depuis un fd (readv)
    FdFdstatGet,        //  5 — attrs d'un fd (fcntl F_GETFL)
    EnvironSizesGet,    //  6 — taille des variables d'env
    EnvironGet,         //  7 — variables d'environnement
    FdPrestatGet,       //  8 — prestat d'un fd prémounté
    FdPrestatDirName,   //  9 — nom du répertoire prémonté
    PathOpen,           // 10 — ouvre un chemin (open)
    FdFdstatSetFlags,   // 11 — modifie les flags d'un fd
    ArgsSizesGet,       // 12 — taille des arguments
    ArgsGet,            // 13 — arguments du programme
    ClockTimeGet,       // 14 — horloge (clock_gettime)

    // ── Batch 1 : filesystem stats + readdir ───────────────────
    FdFilestatGet,      // 15 — stat par fd (fstat)
    PathFilestatGet,    // 16 — stat par chemin (stat/lstat)
    FdReaddir,          // 17 — lire un répertoire (readdir)

    // ── Batch 1 : opérations fichiers/dossiers ─────────────────
    PathCreateDirectory, // 18 — créer un dossier (mkdir)
    PathRemoveDirectory, // 19 — supprimer un dossier (rmdir)
    PathUnlinkFile,     // 20 — supprimer un fichier (unlink)
    PathRename,         // 21 — renommer (rename)

    // ── Batch 1 : sync + random ────────────────────────────────
    FdSync,             // 22 — sync fd (fsync) — no-op
    FdDatasync,         // 23 — sync données (fdatasync) — no-op
    SchedYield,         // 24 — céder le CPU (sched_yield)
    RandomGet,          // 25 — données aléatoires (/dev/urandom)

    // ── Batch 2 : liens symboliques et hardlinks ───────────────
    PathSymlink,        // 26 — lien symbolique (symlink)
    PathReadlink,       // 27 — lire un lien symbo (readlink)
    PathLink,           // 28 — hard link (link)

    // ── Batch 2 : timestamps et taille ────────────────────────
    FdFilestatSetSize,  // 29 — tronquer un fichier (ftruncate)
    FdFilestatSetTimes, // 30 — modifier timestamps (futimens)
    PathFilestatSetTimes, // 31 — modifier timestamps par chemin (utimensat)

    // ── Batch 2 : poll/event (base pour select/epoll) ─────────
    PollOneoff,         // 32 — attendre des événements (poll/select)

    // ── Batch 2 : process ─────────────────────────────────────
    ProcRaise,          // 33 — envoyer un signal (raise)

    // ── Batch 2 : socket (base réseau WASI preview1) ───────────
    SockAccept,         // 34 — accepter une connexion (accept)
    SockRecv,           // 35 — recevoir des données (recv)
    SockSend,           // 36 — envoyer des données (send)
    SockShutdown,       // 37 — fermer une socket (shutdown)

    // ── Batch 2 : advisory ────────────────────────────────────
    FdAdvise,           // 38 — conseil d'accès (posix_fadvise)
    FdAllocate,         // 39 — pré-allouer de l'espace (posix_fallocate)
    FdTell,             // 40 — position courante (ftell)
}

impl FromStr for SystemCall {
    type Err = &'static str;

    fn from_str(fn_name: &str) -> Result<Self, Self::Err> {
        match fn_name {
            // Originaux
            "proc_exit"              => Ok(SystemCall::ProcExit),
            "fd_close"               => Ok(SystemCall::FdClose),
            "fd_write"               => Ok(SystemCall::FdWrite),
            "fd_seek"                => Ok(SystemCall::FdSeek),
            "fd_read"                => Ok(SystemCall::FdRead),
            "fd_fdstat_get"          => Ok(SystemCall::FdFdstatGet),
            "environ_sizes_get"      => Ok(SystemCall::EnvironSizesGet),
            "environ_get"            => Ok(SystemCall::EnvironGet),
            "fd_prestat_get"         => Ok(SystemCall::FdPrestatGet),
            "fd_prestat_dir_name"    => Ok(SystemCall::FdPrestatDirName),
            "path_open"              => Ok(SystemCall::PathOpen),
            "fd_fdstat_set_flags"    => Ok(SystemCall::FdFdstatSetFlags),
            "args_sizes_get"         => Ok(SystemCall::ArgsSizesGet),
            "args_get"               => Ok(SystemCall::ArgsGet),
            "clock_time_get"         => Ok(SystemCall::ClockTimeGet),
            // Batch 1
            "fd_filestat_get"        => Ok(SystemCall::FdFilestatGet),
            "path_filestat_get"      => Ok(SystemCall::PathFilestatGet),
            "fd_readdir"             => Ok(SystemCall::FdReaddir),
            "path_create_directory"  => Ok(SystemCall::PathCreateDirectory),
            "path_remove_directory"  => Ok(SystemCall::PathRemoveDirectory),
            "path_unlink_file"       => Ok(SystemCall::PathUnlinkFile),
            "path_rename"            => Ok(SystemCall::PathRename),
            "fd_sync"                => Ok(SystemCall::FdSync),
            "fd_datasync"            => Ok(SystemCall::FdDatasync),
            "sched_yield"            => Ok(SystemCall::SchedYield),
            "random_get"             => Ok(SystemCall::RandomGet),
            // Batch 2
            "path_symlink"           => Ok(SystemCall::PathSymlink),
            "path_readlink"          => Ok(SystemCall::PathReadlink),
            "path_link"              => Ok(SystemCall::PathLink),
            "fd_filestat_set_size"   => Ok(SystemCall::FdFilestatSetSize),
            "fd_filestat_set_times"  => Ok(SystemCall::FdFilestatSetTimes),
            "path_filestat_set_times"=> Ok(SystemCall::PathFilestatSetTimes),
            "poll_oneoff"            => Ok(SystemCall::PollOneoff),
            "proc_raise"             => Ok(SystemCall::ProcRaise),
            "sock_accept"            => Ok(SystemCall::SockAccept),
            "sock_recv"              => Ok(SystemCall::SockRecv),
            "sock_send"              => Ok(SystemCall::SockSend),
            "sock_shutdown"          => Ok(SystemCall::SockShutdown),
            "fd_advise"              => Ok(SystemCall::FdAdvise),
            "fd_allocate"            => Ok(SystemCall::FdAllocate),
            "fd_tell"                => Ok(SystemCall::FdTell),
            _ => Err("Unknown WASI system call."),
        }
    }
}

impl TryFrom<usize> for SystemCall {
    type Error = &'static str;

    fn try_from(syscall_index: usize) -> Result<Self, Self::Error> {
        match syscall_index {
            // Originaux
            0  => Ok(SystemCall::ProcExit),
            1  => Ok(SystemCall::FdClose),
            2  => Ok(SystemCall::FdWrite),
            3  => Ok(SystemCall::FdSeek),
            4  => Ok(SystemCall::FdRead),
            5  => Ok(SystemCall::FdFdstatGet),
            6  => Ok(SystemCall::EnvironSizesGet),
            7  => Ok(SystemCall::EnvironGet),
            8  => Ok(SystemCall::FdPrestatGet),
            9  => Ok(SystemCall::FdPrestatDirName),
            10 => Ok(SystemCall::PathOpen),
            11 => Ok(SystemCall::FdFdstatSetFlags),
            12 => Ok(SystemCall::ArgsSizesGet),
            13 => Ok(SystemCall::ArgsGet),
            14 => Ok(SystemCall::ClockTimeGet),
            // Batch 1
            15 => Ok(SystemCall::FdFilestatGet),
            16 => Ok(SystemCall::PathFilestatGet),
            17 => Ok(SystemCall::FdReaddir),
            18 => Ok(SystemCall::PathCreateDirectory),
            19 => Ok(SystemCall::PathRemoveDirectory),
            20 => Ok(SystemCall::PathUnlinkFile),
            21 => Ok(SystemCall::PathRename),
            22 => Ok(SystemCall::FdSync),
            23 => Ok(SystemCall::FdDatasync),
            24 => Ok(SystemCall::SchedYield),
            25 => Ok(SystemCall::RandomGet),
            // Batch 2
            26 => Ok(SystemCall::PathSymlink),
            27 => Ok(SystemCall::PathReadlink),
            28 => Ok(SystemCall::PathLink),
            29 => Ok(SystemCall::FdFilestatSetSize),
            30 => Ok(SystemCall::FdFilestatSetTimes),
            31 => Ok(SystemCall::PathFilestatSetTimes),
            32 => Ok(SystemCall::PollOneoff),
            33 => Ok(SystemCall::ProcRaise),
            34 => Ok(SystemCall::SockAccept),
            35 => Ok(SystemCall::SockRecv),
            36 => Ok(SystemCall::SockSend),
            37 => Ok(SystemCall::SockShutdown),
            38 => Ok(SystemCall::FdAdvise),
            39 => Ok(SystemCall::FdAllocate),
            40 => Ok(SystemCall::FdTell),
            _ => Err("Unknown WASI system call."),
        }
    }
}

impl From<SystemCall> for usize {
    fn from(val: SystemCall) -> Self {
        match val {
            // Originaux
            SystemCall::ProcExit            =>  0,
            SystemCall::FdClose             =>  1,
            SystemCall::FdWrite             =>  2,
            SystemCall::FdSeek              =>  3,
            SystemCall::FdRead              =>  4,
            SystemCall::FdFdstatGet         =>  5,
            SystemCall::EnvironSizesGet     =>  6,
            SystemCall::EnvironGet          =>  7,
            SystemCall::FdPrestatGet        =>  8,
            SystemCall::FdPrestatDirName    =>  9,
            SystemCall::PathOpen            => 10,
            SystemCall::FdFdstatSetFlags    => 11,
            SystemCall::ArgsSizesGet        => 12,
            SystemCall::ArgsGet             => 13,
            SystemCall::ClockTimeGet        => 14,
            // Batch 1
            SystemCall::FdFilestatGet       => 15,
            SystemCall::PathFilestatGet     => 16,
            SystemCall::FdReaddir           => 17,
            SystemCall::PathCreateDirectory => 18,
            SystemCall::PathRemoveDirectory => 19,
            SystemCall::PathUnlinkFile      => 20,
            SystemCall::PathRename          => 21,
            SystemCall::FdSync              => 22,
            SystemCall::FdDatasync          => 23,
            SystemCall::SchedYield          => 24,
            SystemCall::RandomGet           => 25,
            // Batch 2
            SystemCall::PathSymlink         => 26,
            SystemCall::PathReadlink        => 27,
            SystemCall::PathLink            => 28,
            SystemCall::FdFilestatSetSize   => 29,
            SystemCall::FdFilestatSetTimes  => 30,
            SystemCall::PathFilestatSetTimes => 31,
            SystemCall::PollOneoff          => 32,
            SystemCall::ProcRaise           => 33,
            SystemCall::SockAccept          => 34,
            SystemCall::SockRecv            => 35,
            SystemCall::SockSend            => 36,
            SystemCall::SockShutdown        => 37,
            SystemCall::FdAdvise            => 38,
            SystemCall::FdAllocate          => 39,
            SystemCall::FdTell              => 40,
        }
    }
}

impl From<SystemCall> for Signature {
    fn from(val: SystemCall) -> Self {
        match val {
            // Originaux
            SystemCall::ProcExit            => sig!((I32)),
            SystemCall::FdClose             => sig!((I32)->I32),
            SystemCall::FdWrite             => sig!((I32,I32,I32,I32)->I32),
            SystemCall::FdSeek              => sig!((I32,I64,I32,I32)->I32),
            SystemCall::FdRead              => sig!((I32,I32,I32,I32)->I32),
            SystemCall::FdFdstatGet         => sig!((I32,I32)->I32),
            SystemCall::EnvironSizesGet     => sig!((I32,I32)->I32),
            SystemCall::EnvironGet          => sig!((I32,I32)->I32),
            SystemCall::FdPrestatGet        => sig!((I32,I32)->I32),
            SystemCall::FdPrestatDirName    => sig!((I32,I32,I32)->I32),
            SystemCall::PathOpen            => sig!((I32,I32,I32,I32,I32,I64,I64,I32,I32)->I32),
            SystemCall::FdFdstatSetFlags    => sig!((I32,I32)->I32),
            SystemCall::ArgsSizesGet        => sig!((I32,I32)->I32),
            SystemCall::ArgsGet             => sig!((I32,I32)->I32),
            SystemCall::ClockTimeGet        => sig!((I32,I64,I32)->I32),
            // Batch 1
            SystemCall::FdFilestatGet       => sig!((I32,I32)->I32),
            SystemCall::PathFilestatGet     => sig!((I32,I32,I32,I32,I32)->I32),
            SystemCall::FdReaddir           => sig!((I32,I32,I32,I64,I32)->I32),
            SystemCall::PathCreateDirectory => sig!((I32,I32,I32)->I32),
            SystemCall::PathRemoveDirectory => sig!((I32,I32,I32)->I32),
            SystemCall::PathUnlinkFile      => sig!((I32,I32,I32)->I32),
            SystemCall::PathRename          => sig!((I32,I32,I32,I32,I32,I32)->I32),
            SystemCall::FdSync              => sig!((I32)->I32),
            SystemCall::FdDatasync          => sig!((I32)->I32),
            SystemCall::SchedYield          => sig!(()->I32),
            SystemCall::RandomGet           => sig!((I32,I32)->I32),
            // Batch 2
            // path_symlink(old_path_ptr, old_path_len, fd, new_path_ptr, new_path_len) -> i32
            SystemCall::PathSymlink         => sig!((I32,I32,I32,I32,I32)->I32),
            // path_readlink(fd, path_ptr, path_len, buf_ptr, buf_len, buf_used_ptr) -> i32
            SystemCall::PathReadlink        => sig!((I32,I32,I32,I32,I32,I32)->I32),
            // path_link(old_fd, old_flags, old_path_ptr, old_path_len, new_fd, new_path_ptr, new_path_len) -> i32
            SystemCall::PathLink            => sig!((I32,I32,I32,I32,I32,I32,I32)->I32),
            // fd_filestat_set_size(fd, size) -> i32
            SystemCall::FdFilestatSetSize   => sig!((I32,I64)->I32),
            // fd_filestat_set_times(fd, atim, mtim, fst_flags) -> i32
            SystemCall::FdFilestatSetTimes  => sig!((I32,I64,I64,I32)->I32),
            // path_filestat_set_times(fd, flags, path_ptr, path_len, atim, mtim, fst_flags) -> i32
            SystemCall::PathFilestatSetTimes => sig!((I32,I32,I32,I32,I64,I64,I32)->I32),
            // poll_oneoff(in_ptr, out_ptr, nsubscriptions, nevents_ptr) -> i32
            SystemCall::PollOneoff          => sig!((I32,I32,I32,I32)->I32),
            // proc_raise(signal) -> i32
            SystemCall::ProcRaise           => sig!((I32)->I32),
            // sock_accept(fd, flags, result_fd_ptr) -> i32
            SystemCall::SockAccept          => sig!((I32,I32,I32)->I32),
            // sock_recv(fd, ri_data_ptr, ri_data_len, ri_flags, ro_datalen_ptr, ro_flags_ptr) -> i32
            SystemCall::SockRecv            => sig!((I32,I32,I32,I32,I32,I32)->I32),
            // sock_send(fd, si_data_ptr, si_data_len, si_flags, so_datalen_ptr) -> i32
            SystemCall::SockSend            => sig!((I32,I32,I32,I32,I32)->I32),
            // sock_shutdown(fd, how) -> i32
            SystemCall::SockShutdown        => sig!((I32,I32)->I32),
            // fd_advise(fd, offset, len, advice) -> i32
            SystemCall::FdAdvise            => sig!((I32,I64,I64,I32)->I32),
            // fd_allocate(fd, offset, len) -> i32
            SystemCall::FdAllocate          => sig!((I32,I64,I64)->I32),
            // fd_tell(fd, offset_ptr) -> i32
            SystemCall::FdTell              => sig!((I32,I32)->I32),
        }
    }
}

/// WASI rights d'un répertoire avec permissions complètes.
pub const FULL_DIR_RIGHTS: wasi::Rights = wasi::RIGHTS_FD_FDSTAT_SET_FLAGS
    | wasi::RIGHTS_FD_SYNC
    | wasi::RIGHTS_FD_ADVISE
    | wasi::RIGHTS_PATH_CREATE_DIRECTORY
    | wasi::RIGHTS_PATH_CREATE_FILE
    | wasi::RIGHTS_PATH_LINK_SOURCE
    | wasi::RIGHTS_PATH_LINK_TARGET
    | wasi::RIGHTS_PATH_OPEN
    | wasi::RIGHTS_FD_READDIR
    | wasi::RIGHTS_PATH_READLINK
    | wasi::RIGHTS_PATH_RENAME_SOURCE
    | wasi::RIGHTS_PATH_RENAME_TARGET
    | wasi::RIGHTS_PATH_FILESTAT_GET
    | wasi::RIGHTS_PATH_FILESTAT_SET_SIZE
    | wasi::RIGHTS_PATH_FILESTAT_SET_TIMES
    | wasi::RIGHTS_FD_FILESTAT_GET
    | wasi::RIGHTS_FD_FILESTAT_SET_SIZE
    | wasi::RIGHTS_FD_FILESTAT_SET_TIMES
    | wasi::RIGHTS_PATH_SYMLINK
    | wasi::RIGHTS_PATH_REMOVE_DIRECTORY
    | wasi::RIGHTS_PATH_UNLINK_FILE
    | wasi::RIGHTS_POLL_FD_READWRITE;

/// WASI rights d'un fichier avec permissions complètes.
pub const FULL_FILE_RIGHTS: wasi::Rights = wasi::RIGHTS_FD_DATASYNC
    | wasi::RIGHTS_FD_READ
    | wasi::RIGHTS_FD_SEEK
    | wasi::RIGHTS_FD_FDSTAT_SET_FLAGS
    | wasi::RIGHTS_FD_SYNC
    | wasi::RIGHTS_FD_TELL
    | wasi::RIGHTS_FD_WRITE
    | wasi::RIGHTS_FD_ADVISE
    | wasi::RIGHTS_FD_ALLOCATE
    | wasi::RIGHTS_FD_FILESTAT_GET
    | wasi::RIGHTS_FD_FILESTAT_SET_SIZE
    | wasi::RIGHTS_FD_FILESTAT_SET_TIMES
    | wasi::RIGHTS_POLL_FD_READWRITE;