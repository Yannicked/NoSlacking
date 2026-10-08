//! The test screen as dma-bufs (`NOSLACKING_VIDEO_TEST_FRAMES=dmabuf`):
//! the benchmark's stand-in for a compositor handing PipeWire buffers
//! over, so the share's GPU path from dma-buf to H.264 can be measured
//! without anyone answering the portal's dialog.
//!
//! The pictures ([`pattern::packed_loop`]) are written once into RGB
//! surfaces of a display of their own (laid out linearly where the
//! driver can, as the portal's buffers are), exported as dma-bufs and
//! handed to the share at the share's rate, round and round.

use std::os::fd::AsFd;

use crate::capture::{self, DmaBuf, Frame, Order, Trouble, pattern};
use crate::pipeline::{Capture, Settings};

use super::va::Display;
use super::va::prime::{Exported, Rgb};

/// Starts the dma-buf test screen.
pub fn test_screen(settings: Settings) -> Result<Capture, Trouble> {
    capture::spawn(
        "noslacking-share-test-dmabuf",
        settings,
        |pipeline, inbox, _feed, started| {
            let made = (|| -> Result<_, String> {
                let display = Display::open()?;
                if !display.exports() {
                    return Err("this libva does not export surfaces".into());
                }
                let mut frames = Vec::new();
                for (width, height, data) in pattern::packed_loop() {
                    let surfaces = display.rgb_surface(width, height, Rgb::BGRX, true)?;
                    display.write_packed(
                        surfaces.ids[0],
                        (width, height),
                        Rgb::BGRX,
                        &data,
                        width as usize * 4,
                    )?;
                    let exported: Exported = display.export(surfaces.ids[0])?;
                    frames.push((surfaces, exported));
                }
                Ok((display, frames))
            })();
            let (_display, frames) = match made {
                Ok(made) => made,
                Err(why) => {
                    let _ = started.send(Err(Trouble::failed(format!(
                        "the dma-buf test screen: {why}"
                    ))));
                    return;
                }
            };
            if let Some((_, first)) = frames.first() {
                eprintln!(
                    "noslacking-video: share: the test screen as {} dma-bufs, modifier {:#x}",
                    frames.len(),
                    first.modifier
                );
            }
            let _ = started.send(Ok(()));
            let mut n = 0usize;
            capture::poll(
                pipeline,
                &inbox,
                || {
                    n += 1;
                    Ok(n)
                },
                |pipeline, n, at| {
                    let (_, exported) = &frames[n % frames.len()];
                    pipeline.put(
                        &Frame::DmaBuf(DmaBuf {
                            fd: exported.fd.as_fd(),
                            width: exported.width,
                            height: exported.height,
                            offset: exported.offset,
                            stride: exported.pitch,
                            order: Order::Bgra,
                            alpha: false,
                            modifier: exported.modifier,
                        }),
                        at,
                    );
                },
            );
        },
    )
}
