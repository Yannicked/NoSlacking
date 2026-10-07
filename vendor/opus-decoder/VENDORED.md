# opus-decoder 0.1.1, patched

A copy of [`opus-decoder`](https://crates.io/crates/opus-decoder) 0.1.1
(<https://github.com/TadeuszWolfGang/Rusopus>, MIT OR Apache-2.0, as its
manifest says), used through `[patch.crates-io]` in NoSlacking's
`Cargo.toml` for huddle audio. Only `src/`, the README and a standalone
manifest are copied; the upstream tests and benches are left out.

Two changes, each marked `NoSlacking:` in the code:

1. `src/lib.rs`, hybrid mode: the redundancy length read from the packet
   is checked against the frame as libopus does (`opus_decoder.c`,
   `if (len*8 < ec_tell(&dec))`). Without it a malformed packet sliced
   out of range and panicked, which in a release build (`panic = "abort"`)
   closes the app; one participant could close everyone's NoSlacking.
2. `src/celt/vq.rs`, `extract_collapse_mask`: a transient frame can have
   16 short blocks, so `1 << i` on a `u8` overflowed (a panic in debug
   builds). `wrapping_shl` keeps what release builds always computed,
   which the fold that follows turns into libopus's mask.

Both are reported upstream (see TODO.md); drop this copy once a release
has them.
