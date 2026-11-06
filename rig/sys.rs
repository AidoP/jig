//! What rig needs of the system which `std` does not provide: interrupts, process groups, the jobserver's FIFO or
//! semaphore, file modes, the size of a terminal and reflinks, each of which has the same interface on each platform.
//!
//! The `unsafe` code of rig is here, behind a safe interface.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
