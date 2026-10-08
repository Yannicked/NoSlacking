//! Capture from the X server, for an X11 session without the ScreenCast
//! portal: `x11rb`'s own connection (pure Rust, no libxcb), the screens
//! from RandR, each picture asked for with GetImage on the root window.
//!
//! Only whole screens: a window's own pixels are not kept by the X server
//! once something covers it (without a compositor), so what GetImage
//! gives of a window is whatever is on top. The portal, where there is
//! one, offers windows too.

use noslacking_video_ipc::{CaptureProblem, ShareChoice, Source, SourceKind};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use super::{Frame, Order, Packed, Trouble};
use crate::pipeline::{Capture, Settings};

/// A screen of the X server: where it is on the root window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// The id of the whole root window in the picker.
const ALL: &str = "x11:all";

/// Each screen (RandR monitor) by name, and all of them together when
/// there are several.
pub fn sources() -> Result<Vec<Source>, Trouble> {
    let (conn, screen) = connect()?;
    let screens = monitors(&conn, screen);
    let mut sources: Vec<Source> = screens
        .iter()
        .map(|(name, area)| Source {
            id: format!("x11:{}:{}:{}:{}", area.x, area.y, area.width, area.height),
            name: format!("{name} ({}×{})", area.width, area.height),
            kind: SourceKind::Screen,
        })
        .collect();
    if sources.len() != 1 {
        let root = &conn.setup().roots[screen];
        sources.push(Source {
            id: ALL.into(),
            name: format!(
                "All screens ({}×{})",
                root.width_in_pixels, root.height_in_pixels
            ),
            kind: SourceKind::Screen,
        });
    }
    Ok(sources)
}

/// Captures the screen `choice` names at the share's rate (`Profile::SHARE`).
pub fn start(choice: &ShareChoice, settings: Settings) -> Result<Capture, Trouble> {
    let id = match choice {
        ShareChoice::Source(id) => id.clone(),
        // No dialog of the system's here: the first screen.
        _ => sources()?
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| Trouble::new(CaptureProblem::Gone, "no screen"))?,
    };
    super::spawn(
        "noslacking-share-x11",
        settings,
        move |pipeline, inbox, _feed, started| {
            let opened = connect().and_then(|(conn, screen)| {
                let root = &conn.setup().roots[screen];
                let area =
                    area_of(&id, root.width_in_pixels, root.height_in_pixels).ok_or_else(|| {
                        Trouble::new(CaptureProblem::Gone, format!("no screen {id:?}"))
                    })?;
                let window = root.root;
                Ok((conn, window, area))
            });
            let (conn, window, area) = match opened {
                Ok(opened) => opened,
                Err(trouble) => {
                    let _ = started.send(Err(trouble));
                    return;
                }
            };
            let _ = started.send(Ok(()));
            eprintln!(
                "noslacking-video: share: capturing {}×{} at {},{} from the X server",
                area.width, area.height, area.x, area.y
            );
            super::poll(
                pipeline,
                &inbox,
                || grab(&conn, window, area),
                |pipeline, (width, height, data), at| {
                    pipeline.put(
                        &Frame::Packed(Packed {
                            width: *width,
                            height: *height,
                            stride: *width as usize * 4,
                            order: Order::Bgra,
                            data,
                        }),
                        at,
                    );
                },
            );
        },
    )
}

/// A connection to the X server and its default screen.
fn connect() -> Result<(RustConnection, usize), Trouble> {
    RustConnection::connect(None)
        .map_err(|e| Trouble::new(CaptureProblem::Unavailable, format!("no X server: {e}")))
}

/// The RandR monitors of `screen`, by name; the whole root if RandR has
/// none to say.
fn monitors(conn: &RustConnection, screen: usize) -> Vec<(String, Area)> {
    let root = &conn.setup().roots[screen];
    let whole = Area {
        x: 0,
        y: 0,
        width: root.width_in_pixels,
        height: root.height_in_pixels,
    };
    let Ok(reply) = conn
        .randr_get_monitors(root.root, true)
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()))
    else {
        return vec![("Screen".into(), whole)];
    };
    let mut out = Vec::new();
    for (n, monitor) in reply.monitors.iter().enumerate() {
        let name = conn
            .get_atom_name(monitor.name)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Screen {}", n + 1));
        out.push((
            name,
            Area {
                x: monitor.x,
                y: monitor.y,
                width: monitor.width,
                height: monitor.height,
            },
        ));
    }
    if out.is_empty() {
        out.push(("Screen".into(), whole));
    }
    out
}

/// The area a picker's id names, within a root of `width`×`height`.
fn area_of(id: &str, width: u16, height: u16) -> Option<Area> {
    if id == ALL {
        return Some(Area {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
    let mut parts = id.strip_prefix("x11:")?.split(':');
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let w: u16 = parts.next()?.parse().ok()?;
    let h: u16 = parts.next()?.parse().ok()?;
    (w >= 16 && h >= 16).then_some(Area {
        x,
        y,
        width: w,
        height: h,
    })
}

/// One picture of `area` of `window`, as the X server has it: 32 bits a
/// pixel, blue first (the usual depth 24 or 32 layout on little-endian
/// servers; anything else is refused).
fn grab(conn: &RustConnection, window: u32, area: Area) -> Result<(u32, u32, Vec<u8>), String> {
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            window,
            area.x,
            area.y,
            area.width,
            area.height,
            u32::MAX,
        )
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let (width, height) = (u32::from(area.width), u32::from(area.height));
    if reply.data.len() != width as usize * height as usize * 4 {
        return Err(format!(
            "a {}-byte picture at depth {} for {width}×{height}",
            reply.data.len(),
            reply.depth
        ));
    }
    Ok((width, height, reply.data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures from the X server in `DISPLAY`: run under a virtual one,
    /// `xvfb-run cargo test -p noslacking-video -- --ignored x11_capture`,
    /// never against someone's screen.
    #[test]
    #[ignore = "needs an X server (Xvfb)"]
    fn x11_capture_lists_screens_and_shares_them() {
        let sources = sources().expect("screens");
        assert!(!sources.is_empty());
        assert!(sources.iter().all(|s| s.kind == SourceKind::Screen));
        let share = start(
            &ShareChoice::Source(sources[0].id.clone()),
            Settings::share(false, 1_000_000, None),
        )
        .expect("started");
        let frame = share
            .next(true, true, std::time::Duration::from_millis(250))
            .expect("no trouble")
            .expect("a frame");
        assert!(frame.keyframe && frame.width >= 16 && frame.height >= 16);
    }

    #[test]
    fn picker_ids_name_their_area() {
        assert_eq!(
            area_of("x11:1920:0:2560:1440", 4480, 1440),
            Some(Area {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440
            })
        );
        assert_eq!(
            area_of(ALL, 4480, 1440),
            Some(Area {
                x: 0,
                y: 0,
                width: 4480,
                height: 1440
            })
        );
        assert_eq!(area_of("x11:0:0:8:8", 100, 100), None);
        assert_eq!(area_of("portal", 100, 100), None);
        assert_eq!(area_of("x11:a:b:c:d", 100, 100), None);
    }
}
