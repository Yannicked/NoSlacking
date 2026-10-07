//! A larger pipe for the pictures. Linux pipes hold 64 KiB by default,
//! so a 3 MB picture crosses in some fifty turns of writer and reader
//! waking each other; at 1 MiB it takes three. Measured on a Ryzen AI 7
//! 350: a decoded 1080p picture back through the helper went from about
//! 6.6 ms to 5.3 ms a frame with the pipe enlarged. Standard input is
//! left at 64 KiB: a 1080p picture to encode took 5.2 ms through it and
//! 11.8 ms through a 1 MiB one, the app's writer no longer overlapping
//! the helper's reading.

/// Asks for a 1 MiB pipe on standard output (the most an unprivileged
/// process may ask by default); if it is not a pipe or the kernel says
/// no, the pipe stays as it is.
pub fn enlarge_stdout() {
    const SIZE: libc::c_int = 1 << 20;
    // SAFETY: fcntl on descriptor 1 with F_SETPIPE_SZ takes an int and
    // touches no memory of ours; on anything but a pipe it fails with
    // EBADF or EINVAL, which is ignored.
    let _ = unsafe { libc::fcntl(1, libc::F_SETPIPE_SZ, SIZE) };
}
