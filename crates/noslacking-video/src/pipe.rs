//! A larger pipe for the pictures. Linux pipes hold 64 KiB by default,
//! so a 3 MB picture crosses in some fifty turns of writer and reader
//! waking each other; at 1 MiB it takes three. Measured on a Ryzen 7
//! 350: a 1080p picture through the helper went from about 6.6 ms to
//! TODO ms a frame with the pipe enlarged.

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
