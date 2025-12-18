use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll, Wake, Waker},
};

use slab::Slab;

use crate::{
    flags::{EXPIRED, NOTIFIED, POLLING},
    task::{Header, Task},
};

mod flags;
mod task;

/// Schedule policy for notified tasks.
pub trait Schedule: Sized {
    /// Schedules a notified task.
    ///
    /// This method can be called from any thread.
    fn schedule(notified: Notified<Self>);

    /// Schedules a notified task (local version).
    ///
    /// This method only be called from the local thread.
    fn schedule_local(notified: Notified<Self>) {
        Self::schedule(notified)
    }
}

/// A token for a notified task.
pub struct Notified<S>(Arc<Header<S>>);

impl<S> Notified<S> {
    /// Returns the schedule data.
    pub fn schedule_data(&self) -> &S {
        &self.0.schedule
    }
}

/// A set of active tasks.
pub struct ActiveTasks<'a, S> {
    /// Slab allocator for tasks.
    slab: UnsafeCell<Slab<Task<'a, S>>>,

    /// Makes this type !Send and !Sync.
    _local_marker: PhantomData<*const ()>,
}

impl<'a, S> ActiveTasks<'a, S> {
    /// Create a new `ActiveTasks`.
    pub const fn new() -> Self {
        Self {
            slab: UnsafeCell::new(Slab::new()),
            _local_marker: PhantomData,
        }
    }

    /// Returns the number of tasks.
    pub fn len(&self) -> usize {
        // SAFETY: No reentrant access to the slab.
        unsafe { self.with_slab_mut(|slab| slab.len()) }
    }

    /// Returns `true` if there are no tasks.
    pub fn is_empty(&self) -> bool {
        // SAFETY: No reentrant access to the slab.
        unsafe { self.with_slab_mut(|slab| slab.is_empty()) }
    }

    /// Clears all tasks.
    pub fn clear(&self) {
        // Dropping a task's future may call back into `ActiveTasks` and mutate
        // the slab (e.g. spawning new tasks). Holding a long-lived borrow would
        // therefore be invalid, so we take the slab out and drop.
        //
        // Newly spawned tasks may be inserted while existing tasks are being dropped,
        // so we must loop until the task set is empty.
        while !self.is_empty() {
            self.mark_all_expired();
            self.take_slab();
        }
    }

    /// Marks all tasks in the slab as expired.
    fn mark_all_expired(&self) {
        // SAFETY: No reentrant access to the slab.
        unsafe {
            self.with_slab_mut(|slab| {
                slab.iter().for_each(|(_, task)| {
                    task.header().state.fetch_or(EXPIRED, Ordering::Relaxed);
                });
            });
        }
    }

    /// Takes the slab, leaving an empty one in its place.
    fn take_slab(&self) -> Slab<Task<'a, S>> {
        // SAFETY: No reentrant access to the slab.
        unsafe { self.with_slab_mut(|slab| mem::take(slab)) }
    }

    /// Provides exclusive mutable access to the slab.
    ///
    /// # Safety
    ///
    /// During the execution of the closure, the slab must be accessed
    /// **only** through the reference passed to the closure.
    /// The closure must not re-enter slab access via any captured
    /// variables or indirect calls.
    unsafe fn with_slab_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Slab<Task<'a, S>>) -> R,
    {
        // SAFETY:
        // Exclusive mutable access is guaranteed because:
        // 1. `Self` is `!Sync`, so no other thread can call this method concurrently.
        // 2. The mutable reference is only used within the scope of the closure.
        // 3. See safety contract of this method.
        f(unsafe { &mut *self.slab.get() })
    }
}

impl<'a, S> ActiveTasks<'a, S>
where
    S: Schedule,
{
    /// Spawns a new task.
    pub fn spawn<F>(&self, schedule: S, future: F)
    where
        F: Future<Output = ()> + 'a,
    {
        // Create a task and insert it into the slab.
        //
        // SAFETY: No reentrant access to the slab.
        let task = unsafe {
            self.with_slab_mut(|slab| {
                let entry = slab.vacant_entry();

                // Create a task with initial state `NOTIFIED`, because we notify it immediately.
                let task = Task::new(NOTIFIED, entry.key(), schedule, future);

                entry.insert(task).clone()
            })
        };

        // Now the task is visible, notify the user to poll it for the first time.
        //
        // SAFETY: `ActiveTasks` is `!Send` and `!Sync`, so this is always called
        // from the local thread.
        unsafe { task.header().clone().notify_local() };
    }
}

impl<'a, S> ActiveTasks<'a, S>
where
    S: Schedule + Send + Sync + 'static,
{
    /// Polls a notified task.
    pub fn poll(&self, notified: Notified<S>) {
        let Notified(header) = notified;

        // TODO: state check

        // Get the task from the slab.
        let task = {
            let get_task = |slab: &mut Slab<Task<'a, S>>| {
                // SAFETY: The task has been checked to be not expired, so the key
                // is guaranteed to refer to a valid slab entry.
                unsafe { slab.get_unchecked(header.key) }.clone()
            };

            // SAFETY: No reentrant access to the slab.
            unsafe { self.with_slab_mut(get_task) }
        };

        // Poll the future.
        //
        // SAFETY:
        // Re-entry into this task’s `poll` method from the inner future’s `poll`
        // method is prevented by the state check at entry to `ActiveTasks::poll`.
        let poll = unsafe { task.poll(&mut Context::from_waker(&Waker::from(header))) };

        // Update the task state based on the poll result.
        match poll {
            Poll::Ready(_) => {
                let header = task.header();

                // Mark `EXPIRED`.
                let old_state = header.state.fetch_or(EXPIRED, Ordering::Relaxed);

                // The task may be marked as expired and removed from slab while
                // being polled due to re-entrant calls into `ActiveTasks`, so we
                // check if it was already expired before removing it.
                if (old_state & EXPIRED) == 0 {
                    // SAFETY: No reentrant access to the slab.
                    unsafe { self.with_slab_mut(|slab| slab.remove(header.key)) };
                }
            }
            Poll::Pending => {
                let header = task.header();

                // Clear `POLLING`.
                let old_state = header.state.fetch_and(!POLLING, Ordering::Relaxed);

                // While the task is polling, wakes only set `NOTIFIED` and do not immediately
                // call the handler. Any wake observed during polling is handled here,
                // unless the task has already expired.
                if old_state & NOTIFIED != 0 && old_state & EXPIRED == 0 {
                    // SAFETY: `ActiveTasks` is `!Send` and `!Sync`, so this is
                    // called from the local thread.
                    unsafe { header.clone().notify_local() };
                }
            }
        }
    }
}

impl<S> Drop for ActiveTasks<'_, S> {
    fn drop(&mut self) {
        self.mark_all_expired();
    }
}

impl<S> Header<S>
where
    S: Schedule,
{
    /// Notifies the task.
    fn notify(self: Arc<Self>) {
        S::schedule(Notified(self))
    }

    /// Notifies the task (local version).
    ///
    /// # Safety
    ///
    /// This method must only be called from the same thread where the task was created.
    unsafe fn notify_local(self: Arc<Self>) {
        S::schedule_local(Notified(self))
    }
}

impl<S> Wake for Header<S>
where
    S: Schedule + Send + Sync + 'static,
{
    fn wake(self: Arc<Self>) {
        // Mark `NOTIFIED`.
        let old_state = self.state.fetch_or(NOTIFIED, Ordering::Relaxed);

        // If the task was not in polling and expired and not notified before, notify it.
        if old_state & (NOTIFIED | POLLING | EXPIRED) == 0 {
            // `Waker` can wake a task from any thread, so we use the non-local notify.
            self.notify();
        }
    }
}
