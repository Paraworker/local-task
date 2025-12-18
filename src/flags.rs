/// The task is notified to be polled.
///
/// Can be set from any thread, but only cleared from the local thread.
pub(crate) const NOTIFIED: u8 = 1 << 0;

/// The task is polling.
///
/// Can be set and cleared only from the local thread.
pub(crate) const POLLING: u8 = 1 << 1;

/// The task has expired.
///
/// Can be set only from the local thread, and never cleared.
pub(crate) const EXPIRED: u8 = 1 << 2;
