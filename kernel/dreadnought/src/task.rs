//! Asynchronous tasks based on Theseus's native OS task subsystem.

use alloc::boxed::Box;
use core::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use task::{ExitValue, JoinableTaskRef, KillReason};

/// Spawns a new asynchronous task.
pub fn spawn_async<F>(
    future: F,
) -> core::result::Result<JoinableAsyncTaskRef<F::Output>, &'static str>
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    let future = Box::pin(future);
    let task = spawn::new_task_builder(crate::block_on, future).spawn()?;
    Ok(JoinableAsyncTaskRef {
        task,
        phantom_data: PhantomData,
    })
}

/// An owned permission to join an async task.
pub struct JoinableAsyncTaskRef<T> {
    pub(crate) task: JoinableTaskRef,
    pub(crate) phantom_data: PhantomData<T>,
}

impl<T> JoinableAsyncTaskRef<T> {
    /// Abort the task. Does not unwind.
    pub fn abort(&self) {
        // Mark as killed directly — .kill() doesn't exist in mai_os.
        use task::{RunState, ExitValue as EV};
        self.task.runstate.store(RunState::Exited(EV::Killed(KillReason::Requested)));
        task::scheduler::remove_task(&self.task);
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.task.runstate.load(), task::RunState::Exited(_))
    }
}

impl<T: Send + 'static> Future for JoinableAsyncTaskRef<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        // NOTE: set_waker is not implemented in mai_os.
        // The executor in `block_on` relies on the blocker/waker pair instead.
        // Here we simply check if the task is done.
        if self.is_finished() {
            Poll::Ready(match self.task.join() {
                Ok(exit_value) => match exit_value {
                    ExitValue::Completed(status) => {
                        // ExitValue::Completed contains isize in mai_os.
                        // We can't recover the original T here without Box<dyn Any>,
                        // so we return an error if status != 0.
                        if status == 0 {
                            Err(Error::Join("cannot downcast isize to T; use block_on instead"))
                        } else {
                            Err(Error::Join("task exited with non-zero status"))
                        }
                    }
                    ExitValue::Killed(reason) => match reason {
                        KillReason::Requested  => Err(Error::Cancelled),
                        KillReason::Panic      => Err(Error::Panic),
                        KillReason::Exception(num) => Err(Error::Exception(num)),
                    },
                },
                Err(s) => Err(Error::Join(s)),
            })
        } else {
            Poll::Pending
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// An error returned from polling a [`JoinableAsyncTaskRef`].
#[derive(Debug)]
pub enum Error {
    Cancelled,
    /// Task panicked. (No PanicInfo available in mai_os's KillReason.)
    Panic,
    /// A join error indicates a BUG in task management.
    Join(&'static str),
    Exception(u8),
}
