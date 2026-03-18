//! This crate creates the abstraction of `stdio`. They are essentially ring buffer of bytes.
//! It also creates the queue for `KeyEvent`, which allows applications to have direct access
//! to keyboard events.
#![no_std]

extern crate alloc;
extern crate spin;
extern crate core2;
extern crate keycodes_ascii;
extern crate sync_irq;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use sync_irq::{Mutex, MutexGuard};
use core2::io::{Read, Write};
use keycodes_ascii::KeyEvent;
use core::ops::Deref;

/// A ring buffer with an EOF mark.
pub struct RingBufferEof<T> {
    /// The ring buffer.
    queue: VecDeque<T>,
    /// The EOF mark. We meet EOF when it equals `true`.
    end: bool
}

/// A reference to a ring buffer with an EOF mark with mutex protection.
///
/// Uses `sync_irq::Mutex` which disables interrupts while the lock is held.
/// This prevents preemption deadlocks: if a reader holds this lock and gets
/// preempted, a writer on the same CPU would spin forever on a plain
/// spin::Mutex. Disabling interrupts prevents timer IRQ-driven context
/// switches during the brief critical section.
pub type RingBufferEofRef<T> = Arc<sync_irq::Mutex<RingBufferEof<T>>>;

/// A ring buffer containing bytes. It forms `stdin`, `stdout` and `stderr`.
/// The two `Arc`s actually point to the same ring buffer. It is designed to prevent
/// interleaved reading but at the same time allow writing to the ring buffer while
/// the reader is holding its lock, and vice versa.
///
/// The outer locks (read_access, write_access) use plain spin::Mutex because
/// they can be held for extended periods (reader busy-waits for data).
/// The inner lock (RingBufferEof) uses sync_irq::Mutex (interrupt-disabling)
/// because it's held briefly and prevents same-CPU preemption deadlocks.
pub struct Stdio {
    /// This prevents interleaved reading.
    read_access: Arc<Mutex<RingBufferEofRef<u8>>>,
    /// This prevents interleaved writing.
    write_access: Arc<Mutex<RingBufferEofRef<u8>>>
}

/// A reader to stdio buffers.
#[derive(Clone)]
pub struct StdioReader {
    /// Inner buffer to support buffered read.
    inner_buf: Box<[u8]>,
    /// The length of actual buffered bytes.
    inner_content_len: usize,
    /// Points to the ring buffer.
    read_access: Arc<Mutex<RingBufferEofRef<u8>>>
}

/// A writer to stdio buffers.
#[derive(Clone)]
pub struct StdioWriter {
    /// Points to the ring buffer.
    write_access: Arc<Mutex<RingBufferEofRef<u8>>>
}

/// `StdioReadGuard` acts like `MutexGuard`, it locks the underlying ring buffer during its
/// lifetime, and provides reading methods to the ring buffer. The lock will be automatically
/// released on dropping of this structure.
pub struct StdioReadGuard<'a> {
    guard: MutexGuard<'a, RingBufferEofRef<u8>>
}

/// `StdioReadGuard` acts like `MutexGuard`, it locks the underlying ring buffer during its
/// lifetime, and provides writing methods to the ring buffer. The lock will be automatically
/// released on dropping of this structure.
pub struct StdioWriteGuard<'a> {
    guard: MutexGuard<'a, RingBufferEofRef<u8>>
}

impl<T> RingBufferEof<T> {
    /// Create a new ring buffer with pre-allocated capacity.
    ///
    /// Pre-allocating avoids heap allocation during write (VecDeque::push_back).
    /// The heap allocator uses hold_preemption() (CLS allocator) which creates
    /// a PreemptionGuard. If the task migrates between CPUs before the guard
    /// is dropped, PreemptionGuard::drop panics on the CPU ID mismatch.
    /// By pre-allocating, push_back doesn't need to grow the buffer for
    /// typical output sizes.
    fn new() -> RingBufferEof<T> {
        RingBufferEof {
            queue: VecDeque::with_capacity(4096),
            end: false
        }
    }
}

impl Stdio {
    /// Create a new stdio buffer.
    pub fn new() -> Stdio {
        let ring_buffer = Arc::new(sync_irq::Mutex::new(RingBufferEof::new()));
        Stdio {
            read_access: Arc::new(Mutex::new(Arc::clone(&ring_buffer))),
            write_access: Arc::new(Mutex::new(ring_buffer))
        }
    }

    /// Get a reader to the stdio buffer. Note that each reader has its own
    /// inner buffer. The buffer size is set to be 256 bytes. Resort to
    /// `get_reader_with_buf_capacity` if one needs a different buffer size.
    pub fn get_reader(&self) -> StdioReader {
        StdioReader {
            inner_buf: Box::new([0u8; 256]),
            inner_content_len: 0,
            read_access: Arc::clone(&self.read_access)
        }
    }

    /// Get a reader to the stdio buffer with a customized buffer size.
    /// Note that each reader has its own inner buffer.
    pub fn get_reader_with_buf_capacity(&self, capacity: usize) -> StdioReader {
        let mut inner_buf = Vec::with_capacity(capacity);
        inner_buf.resize(capacity, 0u8);
        StdioReader {
            inner_buf: inner_buf.into_boxed_slice(),
            inner_content_len: 0,
            read_access: Arc::clone(&self.read_access)
        }
    }

    /// Get a writer to the stdio buffer.
    pub fn get_writer(&self) -> StdioWriter {
        StdioWriter {
            write_access: Arc::clone(&self.write_access)
        }
    }
}

impl StdioReader {
    /// Lock the reader and return a guard that can perform reading operation to that buffer.
    /// Note that this lock does not lock the underlying ring buffer. It only excludes other
    /// readr from performing simultaneous read, but does *not* prevent a writer to perform
    /// writing to the underlying ring buffer.
    pub fn lock(&self) -> StdioReadGuard {
        StdioReadGuard {
            guard: self.read_access.lock()
        }
    }

    /// Read a line from the ring buffer and return. Remaining bytes are stored in the inner
    /// buffer. Do NOT use this function alternatively with `read()` method defined in
    /// `StdioReadGuard`. This function returns the number of bytes read. It will return
    /// zero only upon EOF.
    pub fn read_line(&mut self, buf: &mut String) -> Result<usize, core2::io::Error> {
        let mut total_cnt = 0usize;    // total number of bytes read this time
        let mut new_cnt;               // number of bytes returned from a `read()` invocation
        let mut tmp_buf = Vec::new();  // temporary buffer
        let mut line_finished = false; // mark if we have finished a line

        // Copy from the inner buffer. Process the remaining characters from last read first.
        tmp_buf.resize(self.inner_buf.len(), 0);
        tmp_buf[0..self.inner_content_len].clone_from_slice(&self.inner_buf[0..self.inner_content_len]);
        new_cnt = self.inner_content_len;
        self.inner_content_len = 0;

        loop {
            // Try to find an '\n' character.
            let mut cnt_before_new_line = new_cnt;
            for (idx, c) in tmp_buf[0..new_cnt].iter().enumerate() {
                if *c as char == '\n' {
                    cnt_before_new_line = idx + 1;
                    line_finished = true;
                    break;
                }
            }

            // Append new characters to output buffer (until '\n').
            total_cnt += cnt_before_new_line;
            let new_str = String::from_utf8_lossy(&tmp_buf[0..cnt_before_new_line]);
            buf.push_str(&new_str);

            // If we have read a whole line, copy any byte left to inner buffer, and then return.
            if line_finished {
                self.inner_buf[0..new_cnt-cnt_before_new_line].clone_from_slice(&tmp_buf[cnt_before_new_line..new_cnt]);
                self.inner_content_len = new_cnt - cnt_before_new_line;
                return Ok(total_cnt);
            }

            // We have not finished a whole line. Try to read more from the ring buffer, until
            // we hit EOF.
            let mut locked = self.lock();
            new_cnt = locked.read(&mut tmp_buf[..])?;
            if new_cnt == 0 && locked.is_eof() { return Ok(total_cnt); }
        }
    }
}

impl StdioWriter {
    /// Lock the writer and return a guard that can perform writing operation to that buffer.
    /// Note that this lock does not lock the underlying ring buffer. It only excludes other
    /// writer from performing simultaneous write, but does *not* prevent a reader to perform
    /// reading to the underlying ring buffer.
    pub fn lock(&self) -> StdioWriteGuard {
        StdioWriteGuard {
            guard: self.write_access.lock()
        }
    }
}

impl<'a> Read for StdioReadGuard<'a> {
    /// Read from the ring buffer. Returns the number of bytes read. 
    /// 
    /// Currently it is not possible to return an error, 
    /// but one should *not* assume that because it is subject to change in the future.
    /// 
    /// Note that this method will block until at least one byte is available to be read.
    /// It will only return zero under one of two scenarios:
    /// 1. The EOF flag has been set.
    /// 2. The buffer specified was 0 bytes in length.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, core2::io::Error> {

        // Deal with the edge case that the buffer specified was 0 bytes in length.
        if buf.len() == 0 { return Ok(0); }

        let mut cnt: usize = 0;
        loop {
            let end; // EOF flag
            {
                let mut locked_ring_buf = self.guard.lock();
                let mut buf_iter = buf[cnt..].iter_mut();

                // Keep reading if we have empty space in the output buffer
                // and available byte in the ring buffer.
                while let Some(buf_entry) = buf_iter.next() {
                    if let Some(queue_elem) = locked_ring_buf.queue.pop_front() {
                        *buf_entry = queue_elem;
                        cnt += 1;
                    } else {
                        break;
                    }
                }

                end = locked_ring_buf.end;
            } // the lock on the ring buffer is guaranteed to be dropped here

            // Break if we have read something or we encounter EOF.
            if cnt > 0 || end { break; }

            // Yield CPU time before retrying. Without this, the tight loop
            // monopolizes the inner spinlock due to cache-line locality on x86,
            // starving any concurrent writer trying to push data into the queue.
            for _ in 0..64 {
                core::hint::spin_loop();
            }
        }
        return Ok(cnt);
    }
}

impl<'a> StdioReadGuard<'a> {
    /// Same as `read()`, but is non-blocking.
    /// 
    /// Returns `Ok(0)` when the underlying buffer is empty.
    pub fn try_read(&mut self, buf: &mut [u8]) -> Result<usize, core2::io::Error> {

        // Deal with the edge case that the buffer specified was 0 bytes in length.
        if buf.len() == 0 { return Ok(0); }

        let mut buf_iter = buf.iter_mut();
        let mut cnt: usize = 0;
        let mut locked_ring_buf = self.guard.lock();

        // Keep reading if we have empty space in the output buffer
        // and available byte(s) in the ring buffer.
        while let Some(buf_entry) = buf_iter.next() {
            if let Some(queue_elem) = locked_ring_buf.queue.pop_front() {
                *buf_entry = queue_elem;
                cnt += 1;
            } else {
                break;
            }
        }

        return Ok(cnt);
    }

    /// Returns the number of bytes still in the read buffer.
    pub fn remaining_bytes(&self) -> usize {
        return self.guard.lock().queue.len();
    }
}

impl<'a> Write for StdioWriteGuard<'a> {
    /// Write to the ring buffer, returning the number of bytes written.
    ///
    /// Disables interrupts while holding the inner ring buffer lock to prevent
    /// preemption deadlocks: if the reader (on the same CPU) holds the lock and
    /// gets preempted, the writer would spin forever.
    ///
    /// The EOF check and write are done in a single lock acquisition to avoid
    /// TOCTOU races and reduce contention (previously two separate locks).
    fn write(&mut self, buf: &[u8]) -> Result<usize, core2::io::Error> {
        // Single lock acquisition for both EOF check and write to reduce
        // contention (previously two separate lock acquisitions).
        let mut locked_ring_buf = self.guard.lock();
        if locked_ring_buf.end {
            return Err(core2::io::Error::new(core2::io::ErrorKind::UnexpectedEof,
                                           "cannot write to a stream with EOF set"));
        }
        for byte in buf {
            locked_ring_buf.queue.push_back(*byte)
        }
        Ok(buf.len())
    }
    /// The function required by `Write` trait. Currently it performs nothing,
    /// since everything is write directly to the ring buffer in `write` method.
    fn flush(&mut self) -> Result<(), core2::io::Error> {
        Ok(())
    }
}

impl<'a> StdioReadGuard<'a> {
    /// Check if the EOF flag of the queue has been set.
    pub fn is_eof(&self) -> bool {
        self.guard.lock().end
    }
}

impl<'a> StdioWriteGuard<'a> {
    /// Set the EOF flag of the queue to true.
    pub fn set_eof(&mut self) {
        self.guard.lock().end = true;
    }
}

pub struct KeyEventQueue {
    /// A ring buffer storing `KeyEvent`.
    key_event_queue: RingBufferEofRef<KeyEvent>
}

/// A reader to keyevent ring buffer.
#[derive(Clone)]
pub struct KeyEventQueueReader {
    /// Points to the ring buffer storing `KeyEvent`.
    key_event_queue: RingBufferEofRef<KeyEvent>
}

/// A writer to keyevent ring buffer.
#[derive(Clone)]
pub struct KeyEventQueueWriter {
    /// Points to the ring buffer storing `KeyEvent`.
    key_event_queue: RingBufferEofRef<KeyEvent>
}

impl KeyEventQueue {
    /// Create a new ring buffer storing `KeyEvent`.
    pub fn new() -> KeyEventQueue {
        KeyEventQueue {
            key_event_queue: Arc::new(sync_irq::Mutex::new(RingBufferEof::new()))
        }
    }

    /// Get a reader to the ring buffer.
    pub fn get_reader(&self) -> KeyEventQueueReader {
        KeyEventQueueReader {
            key_event_queue: self.key_event_queue.clone()
        }
    }

    /// Get a writer to the ring buffer.
    pub fn get_writer(&self) -> KeyEventQueueWriter {
        KeyEventQueueWriter {
            key_event_queue: self.key_event_queue.clone()
        }
    }
}

impl KeyEventQueueReader {
    /// Try to read a keyevent from the ring buffer. It returns `None` if currently
    /// the ring buffer is empty.
    pub fn read_one(&self) -> Option<KeyEvent> {
        let mut locked_queue = self.key_event_queue.lock();
        locked_queue.queue.pop_front()
    }
}

impl KeyEventQueueWriter {
    /// Push a keyevent into the ring buffer.
    pub fn write_one(&self, key_event: KeyEvent) {
        let mut locked_queue = self.key_event_queue.lock();
        locked_queue.queue.push_back(key_event);
    }
}

/// A structure that allows applications to access keyboard events directly. 
/// When it gets instantiated, it `take`s the reader of the `KeyEventQueue` away from the `shell`, 
/// or whichever entity previously owned the queue.
/// When it goes out of the scope, the taken reader will be automatically returned
/// back to the `shell` or the original owner in its `Drop` routine.
pub struct KeyEventReadGuard {
    /// The taken reader of the `KeyEventQueue`.
    reader: Option<KeyEventQueueReader>,
    /// The closure to be excuted on dropping.
    closure: Box<dyn Fn(&mut Option<KeyEventQueueReader>)>
}

impl KeyEventReadGuard {
    /// Create a new `KeyEventReadGuard`. This function *takes* a reader
    /// to `KeyEventQueue`. Thus, the `reader` will never be `None` until the
    /// `drop()` method.
    pub fn new(
        reader: KeyEventQueueReader,
        closure: Box<dyn Fn(&mut Option<KeyEventQueueReader>)>
    ) -> KeyEventReadGuard {
        KeyEventReadGuard {
            reader: Some(reader),
            closure
        }
    }
}

impl Drop for KeyEventReadGuard {
    /// Returns the reader of `KeyEventQueue` back to the previous owner by executing the closure.
    fn drop(&mut self) {
        (self.closure)(&mut self.reader);
    }
}

impl Deref for KeyEventReadGuard {
    type Target = Option<KeyEventQueueReader>;

    fn deref(&self) -> &Self::Target {
        &self.reader
    }
}
