//! Capture from the X server, for an X11 session without the ScreenCast
//! portal: `x11rb`'s own connection (pure Rust, no libxcb), the screens
//! from RandR, each picture asked for with GetImage on the root window.
//!
//! Only whole screens: a window's own pixels are not kept by the X server
//! once something covers it (without a compositor), so what GetImage
//! gives of a window is whatever is on top. The portal, where there is
//! one, offers windows too.

use x11rb::connection::Connection as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use super::{
    Capturing, Choice, Ended, Frames, Order, Packed, Picture, ShareError, Source, SourceKind,
};

/// A screen of the X server: where it is on the root window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// The X server.
#[derive(Debug)]
pub struct X11 {
    frames: Frames,
}

/// The id of the whole root window in the picker.
const ALL: &str = "x11:all";

impl X11 {
    /// The X server named by `DISPLAY`, its frames going to `frames`.
    pub fn new(frames: Frames) -> Self {
        Self { frames }
    }

    /// Each screen (RandR monitor) by name, and all of them together when
    /// there are several.
    pub fn sources(&mut self) -> Result<Vec<Source>, ShareError> {
        let (conn, screen) = connect()?;
        let screens = monitors(&conn, screen)?;
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

    /// Captures the screen `choice` names at [`super::FPS`].
    pub fn start(&mut self, choice: &Choice, ended: Ended) -> Result<Capturing, ShareError> {
        let id = match choice {
            Choice::Source(id) => id.clone(),
            // No dialog of the system's here: the first screen.
            Choice::System { .. } => self
                .sources()?
                .first()
                .map(|s| s.id.clone())
                .ok_or(ShareError::Gone)?,
        };
        let frames = self.frames.clone();
        let running = super::spawn_capture("noslacking-share-x11", move |stop, started| {
            let opened = connect().and_then(|(conn, screen)| {
                let root = &conn.setup().roots[screen];
                let area = area_of(&id, root.width_in_pixels, root.height_in_pixels)
                    .ok_or(ShareError::Gone)?;
                let window = root.root;
                Ok((conn, window, area))
            });
            let (conn, window, area) = match opened {
                Ok(opened) => opened,
                Err(error) => {
                    let _ = started.send(Err(error));
                    return;
                }
            };
            let _ = started.send(Ok(()));
            log::info!(
                "huddle share: capturing {}×{} at {},{} from the X server",
                area.width,
                area.height,
                area.x,
                area.y
            );
            super::poll_frames(stop, &frames, &ended, || grab(&conn, window, area));
        })?;
        Ok(Capturing::new(running, None))
    }
}

/// A connection to the X server and its default screen.
fn connect() -> Result<(RustConnection, usize), ShareError> {
    RustConnection::connect(None).map_err(|e| {
        log::warn!("huddle share: no X server: {e}");
        ShareError::Unavailable
    })
}

/// The RandR monitors of `screen`, by name; the whole root if RandR has
/// none to say.
fn monitors(conn: &RustConnection, screen: usize) -> Result<Vec<(String, Area)>, ShareError> {
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
        return Ok(vec![("Screen".into(), whole)]);
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
    Ok(out)
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
fn grab(conn: &RustConnection, window: u32, area: Area) -> Result<Picture, String> {
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
    let (width, height) = (usize::from(area.width), usize::from(area.height));
    if reply.data.len() != width * height * 4 {
        return Err(format!(
            "a {}-byte picture at depth {} for {width}×{height}",
            reply.data.len(),
            reply.depth
        ));
    }
    Ok(Picture::Packed(Packed {
        width,
        height,
        stride: width * 4,
        order: Order::Bgra,
        data: reply.data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures from the X server in `DISPLAY`: run under a virtual one,
    /// `xvfb-run cargo test --features huddle-share -- --ignored
    /// x11_capture`, never against someone's screen.
    #[test]
    #[ignore = "needs an X server (Xvfb)"]
    fn x11_capture_lists_screens_and_grabs_frames() {
        let frames = Frames::default();
        let mut x11 = X11::new(frames.clone());
        let sources = x11.sources().expect("screens");
        assert!(!sources.is_empty());
        assert!(sources.iter().all(|s| s.kind == SourceKind::Screen));
        let (ended, _heard) = Ended::new();
        let capturing = x11
            .start(&Choice::Source(sources[0].id.clone()), ended)
            .expect("started");
        let frame = frames
            .take(std::time::Duration::from_secs(3))
            .expect("a frame");
        let (width, height) = frame.picture.size();
        assert!(width >= 16 && height >= 16);
        let sent = frame
            .picture
            .to_send(super::super::MAX_SIZE)
            .expect("sendable");
        assert!(sent.whole());
        drop(capturing);
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
