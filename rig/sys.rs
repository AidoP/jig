//! What rig needs of the system which `std` does not provide: interrupts, process groups, the jobserver's FIFO or
//! semaphore, the shell, file modes, the size of a terminal and reflinks, each of which has the same interface on each
//! platform.
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

#[cfg(test)]
mod tests {
    #[test]
    fn gives_random_bytes() {
        let (mut a, mut b) = ([0; 32], [0; 32]);
        super::random(&mut a).unwrap();
        super::random(&mut b).unwrap();
        // which are, by chance, neither all zero nor the same twice
        assert!(a != [0; 32] && a != b);
    }
}
