use std::{
    cell::UnsafeCell,
    pin::Pin,
    rc::Rc,
    sync::{Arc, atomic::AtomicU8},
    task::{Context, Poll},
};

pub(crate) struct Header<S> {
    /// Task state flags.
    pub(crate) state: AtomicU8,

    /// Key in the slab.
    pub(crate) key: usize,

    /// Schedule data.
    pub(crate) schedule: S,
}

pub(crate) struct Task<'a, S>(Rc<Inner<S, dyn Future<Output = ()> + 'a>>);

struct Inner<S, F: ?Sized> {
    /// Task header.
    header: Arc<Header<S>>,

    /// The future being executed by the task.
    future: UnsafeCell<F>,
}

impl<'a, S> Task<'a, S> {
    /// Creates a new task.
    pub(crate) fn new<F>(state: u8, key: usize, schedule: S, future: F) -> Self
    where
        F: Future<Output = ()> + 'a,
    {
        let header = Arc::new(Header {
            state: AtomicU8::new(state),
            key,
            schedule,
        });

        let inner = Rc::new(Inner {
            header,
            future: UnsafeCell::new(future),
        });

        Self(inner)
    }

    /// Returns the task header.
    pub(crate) fn header(&self) -> &Arc<Header<S>> {
        &self.0.header
    }

    /// Polls the task's future.
    ///
    /// # Safety
    ///
    /// This method must not be re-entered on the same `Task` during the
    /// inner future’s `poll` method.
    pub(crate) unsafe fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        let f = |fut: &mut (dyn Future<Output = ()> + 'a)| {
            // SAFETY:
            // The future is heap-allocated inside an `Rc` and therefore remains at
            // a stable memory location for the duration of its lifetime.
            unsafe { Pin::new_unchecked(fut) }.poll(cx)
        };

        // SAFETY:
        // Any re-entrant access during the execution of the closure
        // can only occur via the inner future's `poll` method.
        // Preventing such re-entry is the caller's responsibility, as stated
        // in the safety contract of this method.
        unsafe { self.0.with_future_mut(f) }
    }
}

impl<S> Clone for Task<'_, S> {
    fn clone(&self) -> Self {
        Task(self.0.clone())
    }
}

impl<S, F: ?Sized> Inner<S, F> {
    /// Provides exclusive mutable access to the inner future.
    ///
    /// # Safety
    ///
    /// During the execution of the closure, the future must be accessed
    /// **only** through the reference passed to the closure.
    /// The closure must not re-enter future access via any captured
    /// variables or indirect calls.
    unsafe fn with_future_mut<R>(&self, f: impl FnOnce(&mut F) -> R) -> R {
        // SAFETY:
        // Exclusive mutable access is guaranteed because:
        // 1. `Self` is `!Sync`, so no other thread can call this method concurrently.
        // 2. The mutable reference is only used within the scope of the closure.
        // 3. See safety contract of this method.
        f(unsafe { &mut *self.future.get() })
    }
}
