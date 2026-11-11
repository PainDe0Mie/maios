//! A basic logger implementation for system-wide logging in Mai.
//!
//! This enables Mai crates to use the [`log`] crate's macros anywhere,
//! such as `error!()`, `warn!()`, `info!()`, `debug!()`, and `trace!()`.
//!
//! Currently, log statements are written to one or more **sinks** ([`LogSink`]),
//! typically the COM1 serial port.
//!
//! Early log messages (before memory management is initialized) are saved
//! to a static fixed-sized buffer such that they are not lost and
//! can be retrieved once logging sinks are ready to be used.
//!
//! # Journal asynchrone
//!
//! Écrire sur un port série en synchrone fait payer à chaque appelant de `warn!` le débit de la
//! ligne : 87 µs par octet à 115 200 bauds, ~10 ms pour une ligne, interruptions coupées. Comme
//! les consoles à fil de Linux (`nbcon`, 6.12), une fois [`start_async`] appelé :
//!
//! * l'appelant met son message en forme **directement dans un anneau** de [`RING_SIZE`] octets
//!   (une copie mémoire sous un verrou court, `PRODUCER`), publie `HEAD`, puis sonne la tâche de
//!   vidage (`log_flusher`) ;
//! * la tâche de vidage ([`drain`]) émet l'anneau par rafales **sans jamais attendre la ligne**
//!   ([`LogSink::write_nonblocking`] : le FIFO du 16550 quand il est vide), et avance `TAIL` ;
//! * aucune perte : un anneau plein fait vider l'anneau par le producteur lui-même, en synchrone
//!   (contre-pression plutôt qu'oubli : le harnais de test lit ce journal) ;
//! * les `error!` restent synchrones : le message est émis avant que l'appelant ne reprenne la
//!   main (une erreur précède souvent un arrêt) ;
//! * les chemins d'urgence (NMI, exceptions) appellent [`flush_emergency`], qui vide l'anneau
//!   sans dépendre de la tâche ni, en dernier recours, du verrou.
//!
//! Ordre des verrous : `PRODUCER` puis `LOGGER` (le consommateur). Le verrou `LOGGER` n'est tenu
//! que le temps d'une rafale, interruptions coupées. Règle : **ne jamais journaliser sous un
//! verrou de file de MaiSched** (la sonnette réveille une tâche, ce qui prend ce verrou).

#![no_std]
#![feature(trait_alias)]

extern crate alloc;
extern crate crossbeam_utils;
extern crate log;
extern crate sync_irq;
extern crate serial_port_basic;

use log::{Record, Level, Metadata, Log};
use core::{
    cell::UnsafeCell,
    fmt::{self, Write},
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use sync_irq::IrqSafeMutex;
use serial_port_basic::SerialPort;
use alloc::{sync::Arc, vec::Vec};

#[cfg(mirror_log_to_vga)]
pub use mirror_log::set_log_mirror_function;

/// By default, Mai will print all log levels, including `Trace` and above.
pub const DEFAULT_LOG_LEVEL: Level = Level::Warn;

/// The maximum number of output streams that a logger can write to.
pub const LOG_MAX_WRITERS: usize = 2;

/// Taille de l'anneau du journal asynchrone (puissance de deux).
pub const RING_SIZE: usize = 1 << 16;

/// Octets émis au plus par appel de [`drain`] (une rafale, verrou tenu).
const DRAIN_BUDGET: usize = 256;

/// The size of the buffer used to save early log messages.
pub const EARLY_LOG_BUFFER_SIZE: usize = {
    #[cfg(target_arch = "x86_64")]  { 0 }
    #[cfg(target_arch = "aarch64")] { 16 * 1024 }
};

/// The early logger used before dynamic heap allocation is available.
static EARLY_LOGGER: IrqSafeMutex<EarlyLogger> = IrqSafeMutex::new(EarlyLogger::new());

/// The early log buffer.
///
/// This is separate from the `EARLY_LOGGER` in order for it to be placed
/// in `.bss` instead of `.data`, saving space in the executable.
static EARLY_LOG_BUFFER: IrqSafeMutex<EarlyLogBuffer<EARLY_LOG_BUFFER_SIZE>> =
    IrqSafeMutex::new(EarlyLogBuffer::new());

/// The real logger instance where log states are kept.
///
/// This is accessed in the [`DummyLogger`]'s log/write methods,
/// it is not called directly by the `log` crate.
/// If `None`, it is uninitialized, and the [`EARLY_LOGGER`] will be used as a fallback.
///
/// En mode asynchrone, ce verrou est aussi celui du **consommateur** de l'anneau.
static LOGGER: IrqSafeMutex<Option<Logger>> = IrqSafeMutex::new(None);

/// A destination for log output, such as a serial port.
///
/// `fmt::Write` sert au mode synchrone (avant [`start_async`]) ; les deux méthodes d'octets
/// servent à l'anneau, qui contient déjà les fins de ligne traduites (`\n` suivi de `\r`).
pub trait LogSink: Write + Send {
    /// Writes all of `bytes`, waiting for the device as needed.
    fn write_blocking(&mut self, bytes: &[u8]);

    /// Writes as many of `bytes` as the device accepts **without waiting** and returns that
    /// count (0 if the device is busy). The default implementation writes everything.
    fn write_nonblocking(&mut self, bytes: &[u8]) -> usize {
        self.write_blocking(bytes);
        bytes.len()
    }
}

/// An early logger that can only write to a fixed number of [`SerialPort`]s,
/// intended for basic use before dynamic heap allocation is available.
struct EarlyLogger([Option<SerialPort>; LOG_MAX_WRITERS]);
impl EarlyLogger {
    const fn new() -> Self {
        const INIT: Option<SerialPort> = None;
        Self([INIT; LOG_MAX_WRITERS])
    }

    /// Initializes this early logger with the given serial port writers.
    ///
    /// Flushes the early log buffer to the newly-added serial ports, if any.
    fn init(&mut self, serial_ports: impl IntoIterator<Item = SerialPort>) {
        let buffer_was_truncated: bool;
        {
            let mut buffer = EARLY_LOG_BUFFER.lock();
            buffer_was_truncated = buffer.truncated;

            let mut added_new_loggers = false;
            for (mut sp, logger_writer) in serial_ports.into_iter()
                .take(LOG_MAX_WRITERS)
                .zip(&mut self.0)
            {
                sp.out_bytes(buffer.get_buf());
                *logger_writer = Some(sp);
                added_new_loggers = true;
            }

            if added_new_loggers {
                buffer.truncate(0);
            }
        }

        if buffer_was_truncated {
            let _ = write!(
                self,
                "\n\n{} \
                ---- (early log was truncated; try increasing logger::EARLY_LOG_BUFFER_SIZE) ----\
                \n{}",
                LogColor::Yellow.as_terminal_string(),
                LogColor::Reset.as_terminal_string(),
            );
        }
    }
}
impl fmt::Write for EarlyLogger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut written = false;
        for serial_port in self.0.iter_mut().flatten() {
            let _ = serial_port.write_str(s);
            written = true;
        }
        if !written {
            let _ = EARLY_LOG_BUFFER.lock().write_str(s);
        }
        Ok(())
    }
}

/// The fully-featured logger that can be dynamically initialized with arbitrary output streams.
///
/// This is the "backend" for the `log` crate that allows Mai to use its `log!()` macros.
struct Logger {
    writers: Vec<Arc<IrqSafeMutex<dyn LogSink>>>,
}

/// Removes all of the writers (output streams) from the early logger and returns them.
///
/// This is intended to allow the caller to take ownership of the early logger writers
/// such that they can switch to initializing the full logger.
pub fn take_early_log_writers() -> [Option<SerialPort>; LOG_MAX_WRITERS] {
    let mut list = [None, None];
    for (opt, ret) in EARLY_LOGGER.lock().0.iter_mut().zip(&mut list) {
        *ret = opt.take();
    }
    list
}

/// A statically-sized buffer for storing early log messages
/// before memory management is initialized.
struct EarlyLogBuffer<const SIZE: usize> {
    array: [u8; SIZE],
    length: usize,
    truncated: bool,
}

impl<const SIZE: usize> EarlyLogBuffer<SIZE> {
    const fn new() -> Self {
        Self {
            array: [0; SIZE],
            length: 0,
            truncated: false,
        }
    }

    /// Shortens this buffer to the given new `length` in bytes.
    ///
    /// If the new `length` exceeds the current buffer length, this does nothing.
    pub fn truncate(&mut self, length: usize) {
        if length <= self.length {
            self.length = length;
            self.truncated = true;
        }
    }

    fn get_buf(&self) -> &[u8] {
        &self.array[0..self.length]
    }
}

impl<const SIZE: usize> fmt::Write for EarlyLogBuffer<SIZE> {
    /// Appends the given string to the buffer.
    ///
    /// Returns an error if there is insufficient space in the buffer.
    ///
    /// Upon a failed write, the caller can truncate the buffer
    /// to its last correct length using [`EarlyLogBuffer::truncate()`].
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let new_length = self.length + s.len();
        if let Some(arr) = self.array.get_mut(self.length .. new_length) {
            arr.copy_from_slice(s.as_bytes());
            self.length = new_length;
            Ok(())
        } else {
            self.length = SIZE;
            self.truncated = true;
            Err(fmt::Error)
        }
    }
}


// ---------------------------------------------------------------------------------------------
// Anneau du journal asynchrone
// ---------------------------------------------------------------------------------------------

/// Octets de l'anneau. Les producteurs (sérialisés par `PRODUCER`) n'écrivent qu'au-delà de
/// `HEAD`, dans la place libre ; le consommateur (sérialisé par `LOGGER`) ne lit qu'entre
/// `TAIL` et `HEAD`. Les deux zones ne se recouvrent jamais.
struct Ring(UnsafeCell<[u8; RING_SIZE]>);
// SAFE : accès partagé réglé par HEAD/TAIL comme décrit ci-dessus.
unsafe impl Sync for Ring {}

static RING: Ring = Ring(UnsafeCell::new([0; RING_SIZE]));
/// Octets publiés depuis le démarrage (compteur monotone ; position = valeur % RING_SIZE).
static HEAD: AtomicUsize = AtomicUsize::new(0);
/// Octets émis depuis le démarrage.
static TAIL: AtomicUsize = AtomicUsize::new(0);
/// Sérialise les producteurs (interruptions coupées : un gestionnaire d'interruption peut
/// journaliser).
static PRODUCER: IrqSafeMutex<()> = IrqSafeMutex::new(());
/// Mode asynchrone actif (la tâche de vidage tourne).
static ASYNC: AtomicBool = AtomicBool::new(false);
/// Sonnette de la tâche de vidage (`fn()` rangée en entier ; 0 = aucune).
static KICK: AtomicUsize = AtomicUsize::new(0);

/// Écrit dans l'anneau à la suite de `HEAD`, en traduisant les fins de ligne comme le pilote
/// série (`\n` → `\n\r`, `\r` → `\r\n`). Ne s'emploie que sous `PRODUCER`.
struct RingWriter {
    head: usize,
}

impl RingWriter {
    #[inline]
    fn push(&mut self, byte: u8) {
        if self.head - TAIL.load(Ordering::Acquire) >= RING_SIZE {
            // Anneau plein : on publie ce qui est écrit, puis on vide nous-mêmes (en
            // synchrone) au lieu de perdre des octets.
            HEAD.store(self.head, Ordering::Release);
            flush_sync();
        }
        // SAFE : cette case est libre (entre HEAD et TAIL + RING_SIZE) et seul le producteur
        // qui tient PRODUCER y écrit.
        unsafe { (*RING.0.get())[self.head % RING_SIZE] = byte; }
        self.head += 1;
    }
}

impl fmt::Write for RingWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            self.push(b);
            if b == b'\n' {
                self.push(b'\r');
            } else if b == b'\r' {
                self.push(b'\n');
            }
        }
        // Publier au fil de l'eau : un message plus long que l'anneau reste possible.
        HEAD.store(self.head, Ordering::Release);
        Ok(())
    }
}

/// Envoie `bytes` aux sorties. La première décide du nombre d'octets acceptés (sans attendre
/// si `blocking` est faux) ; les autres reçoivent les mêmes octets en bloquant.
fn write_sinks(logger: &Logger, bytes: &[u8], blocking: bool) -> usize {
    let mut sinks = logger.writers.iter();
    let first = match sinks.next() {
        Some(s) => s,
        None => return bytes.len(), // aucune sortie : on jette
    };
    let n = {
        let mut sink = first.lock();
        if blocking {
            sink.write_blocking(bytes);
            bytes.len()
        } else {
            let mut off = 0;
            while off < bytes.len() {
                let w = sink.write_nonblocking(&bytes[off..]);
                if w == 0 {
                    break;
                }
                off += w;
            }
            off
        }
    };
    for s in sinks {
        s.lock().write_blocking(&bytes[..n]);
    }
    n
}

/// Émet au plus `budget` octets de l'anneau ; l'appelant tient `LOGGER`. Rend le nombre émis.
fn drain_locked(logger: &Logger, budget: usize, blocking: bool) -> usize {
    let tail = TAIL.load(Ordering::Relaxed);
    let head = HEAD.load(Ordering::Acquire);
    let n = (head - tail).min(budget);
    if n == 0 {
        return 0;
    }
    let start = tail % RING_SIZE;
    let first = n.min(RING_SIZE - start);
    // SAFE : [tail, head) est publié et aucun producteur n'y écrit avant que TAIL n'avance.
    let ring = unsafe { &*RING.0.get() };
    let mut done = write_sinks(logger, &ring[start..start + first], blocking);
    if done == first && n > first {
        done += write_sinks(logger, &ring[..n - first], blocking);
    }
    TAIL.store(tail + done, Ordering::Release);
    done
}

/// Vide tout l'anneau en bloquant (anneau plein, `error!`).
fn flush_sync() {
    if let Some(logger) = &*LOGGER.lock() {
        while drain_locked(logger, usize::MAX, true) > 0 {}
    }
}

/// Résultat d'un appel de [`drain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// L'anneau est vide.
    Empty,
    /// Des octets attendent mais le périphérique est occupé (FIFO plein) : réessayer plus tard.
    Busy,
    /// Des octets ont été émis.
    Progress,
}

/// Émet une rafale de l'anneau sans attendre le périphérique. Appelée par la tâche de vidage.
pub fn drain() -> Drain {
    if !pending() {
        return Drain::Empty;
    }
    match &*LOGGER.lock() {
        Some(logger) => {
            if drain_locked(logger, DRAIN_BUDGET, false) > 0 {
                Drain::Progress
            } else {
                Drain::Busy
            }
        }
        None => Drain::Empty,
    }
}

/// Des octets attendent-ils dans l'anneau ?
#[inline]
pub fn pending() -> bool {
    HEAD.load(Ordering::Acquire) != TAIL.load(Ordering::Acquire)
}

/// Passe le journal en mode asynchrone. `kick` réveille la tâche de vidage ; elle doit être
/// sûre en interruption et ne faire qu'un réveil (pas de journalisation).
pub fn start_async(kick: fn()) {
    KICK.store(kick as usize, Ordering::Release);
    ASYNC.store(true, Ordering::Release);
}

#[inline]
fn kick() {
    let k = KICK.load(Ordering::Acquire);
    if k != 0 {
        // SAFE : KICK ne contient que des `fn()` rangées par `start_async`.
        let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(k) };
        f();
    }
}

/// Vide l'anneau depuis un contexte d'urgence (NMI, exception du noyau, arrêt), sans la tâche.
///
/// Prend le verrou du consommateur s'il se libère vite ; sinon (le code interrompu le tient,
/// par exemple une NMI arrivée pendant une rafale sur ce cœur) écrit directement sur COM1.
pub fn flush_emergency() {
    if !ASYNC.load(Ordering::Acquire) || !pending() {
        return;
    }
    for _ in 0..1_000_000 {
        if let Some(guard) = LOGGER.try_lock() {
            if let Some(logger) = &*guard {
                while drain_locked(logger, usize::MAX, true) > 0 {}
            }
            return;
        }
        core::hint::spin_loop();
    }
    #[cfg(target_arch = "x86_64")]
    {
        let tail = TAIL.load(Ordering::Acquire);
        let head = HEAD.load(Ordering::Acquire);
        let n = (head - tail).min(RING_SIZE);
        let start = tail % RING_SIZE;
        let first = n.min(RING_SIZE - start);
        // SAFE : lecture d'octets publiés ; au pire une ligne en double ou abîmée, en urgence.
        let ring = unsafe { &*RING.0.get() };
        let port = serial_port_basic::SerialPortAddress::COM1;
        serial_port_basic::emergency_out_bytes(port, &ring[start..start + first]);
        serial_port_basic::emergency_out_bytes(port, &ring[..n - first]);
        TAIL.store(tail + n, Ordering::Release);
    }
}


/// The static instance of the dummy logger, as required by the `log` crate.
static DUMMY_LOGGER: DummyLogger = DummyLogger;

/// An empty logger struct used to satisfy the requirements of the `log` crate.
///
/// This exists because the `log` crate only allows a logger implementation
/// to be initialized once from a singleton static instance.
/// To get around that limitation, we store the actual logger states
/// **outside** of the logger struct, such that we can modify them later
/// after the `log` crate has already been initialized.
struct DummyLogger;

impl DummyLogger {
    /// A re-implementation of [`core::fmt::Write::write_fmt()`]
    /// that doesn't require `&mut self`.
    ///
    /// This function writes to the real (fully-featured) [`LOGGER`] if it has been initialized;
    /// otherwise, it falls back to writing to the [`EARLY_LOGGER`] instead.
    ///
    /// En mode asynchrone, le message va dans l'anneau ; `urgent` le fait émettre avant de
    /// rendre la main.
    fn write_fmt(&self, arguments: fmt::Arguments, urgent: bool) -> fmt::Result {
        if ASYNC.load(Ordering::Acquire) {
            {
                let _producer = PRODUCER.lock();
                let mut w = RingWriter { head: HEAD.load(Ordering::Relaxed) };
                let _ = w.write_fmt(arguments);
            }
            if urgent {
                flush_sync();
            } else {
                kick();
            }
            return Ok(());
        }
        if let Some(logger) = &*LOGGER.lock() {
            for writer in logger.writers.iter() {
                let _ = writer.deref().lock().write_fmt(arguments);
            }
        } else {
            let _ = EARLY_LOGGER.lock().write_fmt(arguments);
        }
        // If there was an error above, there's literally nothing we can do but ignore it,
        // because there is no other lower-level way to log errors than this logger.
        Ok(())
    }
}

impl Log for DummyLogger {
    #[inline(always)]
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let (level_str, color) = match record.level() {
            Level::Error => ("[E] ", LogColor::Red),
            Level::Warn =>  ("[W] ", LogColor::Yellow),
            Level::Info =>  ("[I] ", LogColor::Cyan),
            Level::Debug => ("[D] ", LogColor::Green),
            Level::Trace => ("[T] ", LogColor::Purple),
        };
        let file_loc = record.file().unwrap_or("??");
        let line_loc = record.line().unwrap_or(0);
        let _result = self.write_fmt(
            format_args!("{}{}{}:{}: {}{}",
                color.as_terminal_string(),
                level_str,
                file_loc,
                line_loc,
                record.args(),
                LogColor::Reset.as_terminal_string(),
            ),
            record.level() == Level::Error,
        );
        // If there was an error above, there's literally nothing we can do but ignore it,
        // because there is no other lower-level way to log errors than the serial port.

        #[cfg(mirror_log_to_vga)]
        if let Some(func) = mirror_log::get_log_mirror_function() {
            // Currently printing to the VGA terminal doesn't support ANSI color escape sequences,
            // so we exclude the first and the last elements that set those colors.
            func(format_args!("{}{}:{}: {}",
                level_str,
                file_loc,
                line_loc,
                record.args(),
            ));
        }
    }

    fn flush(&self) {
        flush_sync();
    }
}


/// Initializes Mai's early system logger for use before memory management is set up.
///
/// The early logger can save log messages to a statically-sized buffer
/// and can also write log messages to a list of basic serial ports.
///
/// # Arguments
/// * `log_level`: the log level that should be used.
///    If `None`, the [`DEFAULT_LOG_LEVEL`] will be used.
/// * `serial_ports`: an iterator of [`SerialPort`]s that the logger will write log messages to.
///    Typically this is just a single serial port, e.g., `COM1`.
///
/// This function will initialize the logger with a maximum of [`LOG_MAX_WRITERS`] serial ports;
/// any additional ones in the given iterator beyond that will be ignored.
pub fn early_init(
    log_level: Option<Level>,
    serial_ports: impl IntoIterator<Item = SerialPort>,
) {
    // Initialize the early logger. This can be called multiple times,
    // and will flush the saved early log buffer to the new `serial_ports` each time.
    EARLY_LOGGER.lock().init(serial_ports);

    // Tell the `log` crate to use our early logger via the dummy logger.
    // We ignore any errors returned to allow `early_init` to be called multiple times.
    let _ = log::set_logger(&DUMMY_LOGGER);
    set_log_level(log_level.unwrap_or(DEFAULT_LOG_LEVEL));
}


/// Initialize the fully-featured Mai system logger.
///
/// # Arguments
/// * `log_level`: the log level that should be used.
///    If `None`, the [`DEFAULT_LOG_LEVEL`] will be used.
/// * `writers`: an iterator over the backends that the system logger
///    will write log messages to.
///    Typically this is just a single writer, such as the COM1 serial port.
pub fn init<I, W>(log_level: Option<Level>, writers: impl IntoIterator<Item = I>)
where
    W: LogSink + 'static,
    I: Into<Arc<IrqSafeMutex<W>>>,
{
    // Populate the fields of the real logger instance
    let logger = Logger {
        writers: writers.into_iter()
            .map(|i| i.into() as Arc<IrqSafeMutex<dyn LogSink>>)
            .collect::<Vec<_>>(),
    };
    *LOGGER.lock() = Some(logger);

    // Once the real logger has been initialized, tell the `log` crate to use our dummy logger instance.
    // Call `set_logger()` again, just in case we never ran the `early_init()` function;
    // if `early_init()` has already been called, `set_logger()` will return an Error, which is okay.
    let _ = log::set_logger(&DUMMY_LOGGER);
    set_log_level(log_level.unwrap_or(DEFAULT_LOG_LEVEL));
}

/// Set the log level, which determines whether a given log message is actually logged.
///
/// For example, if `Level::Trace` is set, all log levels will be logged.
///
/// If `Level::Info` is set, `debug!()` and `trace!()` will not be logged,
/// but `info!()`, `warn!()`, and `error!()` will be.
pub fn set_log_level(level: Level) {
    log::set_max_level(level.to_level_filter())
}

/// Convenience function for writing formatted arguments to the logger.
///
/// If the logger has not yet been initialized, no log messages will be emitted
/// and an `Error` will be returned.
///
/// Tip: use the `format_args!()` macro from the core library to create
/// the `Arguments` parameter needed here.
pub fn write_fmt(args: fmt::Arguments) -> fmt::Result {
    DUMMY_LOGGER.write_fmt(args, false)
}

/// Convenience function for writing a simple string to the logger.
///
/// If the logger has not yet been initialized, no log messages will be emitted.
/// and an `Error` will be returned.
pub fn write_str(s: &str) -> fmt::Result {
    crate::write_fmt(format_args!("{s}"))
}


/// ANSI style codes for basic colors.
#[allow(dead_code)]
enum LogColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Purple,
    Cyan,
    White,
    Reset,
}
impl LogColor {
    fn as_terminal_string(&self) -> &'static str {
        match *self {
            // \x1b is the ESC character (0x1B)
			LogColor::Black	  =>  "\x1b[30m",
			LogColor::Red	  =>  "\x1b[31m",
			LogColor::Green   =>  "\x1b[32m",
			LogColor::Yellow  =>  "\x1b[33m",
			LogColor::Blue	  =>  "\x1b[34m",
			LogColor::Purple  =>  "\x1b[35m",
            LogColor::Cyan    =>  "\x1b[36m",
            LogColor::White   =>  "\x1b[37m",
            LogColor::Reset   =>  "\x1b[0m\n",
        }
    }
}

#[cfg(mirror_log_to_vga)]
mod mirror_log {
    use core::fmt;
    use crossbeam_utils::atomic::AtomicCell;

    /// Call this to enable mirroring of logger output (e.g., via logging macros)
    /// to another output sink, such as the VGA screen.
    pub fn set_log_mirror_function(func: fn(fmt::Arguments)) {
        MIRROR_LOG_FUNC.store(Some(func));
    }

    pub(crate) fn get_log_mirror_function() -> Option<fn(fmt::Arguments)> {
        MIRROR_LOG_FUNC.load()
    }

    /// The callback function that will optionally be invoked
    /// on every log statement to be printed, which enables log mirroring.
    pub(crate) static MIRROR_LOG_FUNC: AtomicCell<Option<fn(fmt::Arguments)>> = AtomicCell::new(None);
    const _: () = assert!(AtomicCell::<fn(fmt::Arguments)>::is_lock_free());
}
