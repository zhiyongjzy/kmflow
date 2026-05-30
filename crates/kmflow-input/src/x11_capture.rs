use anyhow::{Context, Result};
use kmflow_proto::{ButtonState, InputEvent, KeyState, MouseButton};
use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, GrabMode, GrabStatus};
use x11rb::rust_connection::RustConnection;

use crate::InputCapture;

pub struct X11Capture {
    conn: RustConnection,
    root: xproto::Window,
    captured: bool,
    grab_window: xproto::Window,
}

impl X11Capture {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None).context("connect to X11")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let xi_ext = conn
            .query_extension(b"XInputExtension")
            .context("query xinput")?
            .reply()
            .context("xinput reply")?;

        if !xi_ext.present {
            anyhow::bail!("XInput extension not available");
        }

        // XI2 event mask bits
        // RawKeyPress=13, RawKeyRelease=14, RawButtonPress=15, RawButtonRelease=16, RawMotion=17
        let mask_value: u32 = (1 << 13) | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 17);

        let mask = xinput::EventMask {
            deviceid: xinput::Device::ALL_MASTER.into(),
            mask: vec![mask_value.into()],
        };

        conn.xinput_xi_select_events(root, &[mask])
            .context("select XI events")?;

        let screen_width = screen.width_in_pixels;
        let screen_height = screen.height_in_pixels;

        // Create a tiny 1x1 invisible window for confining the pointer during grab
        let grab_window = conn.generate_id().context("generate window id")?;
        conn.create_window(
            0, // depth: copy from parent
            grab_window,
            root,
            screen_width as i16 / 2,
            screen_height as i16 / 2,
            1,
            1, // 1x1 pixel
            0, // border
            xproto::WindowClass::INPUT_ONLY,
            0, // visual: copy from parent
            &xproto::CreateWindowAux::new().override_redirect(1),
        )
        .context("create grab window")?;

        conn.flush().context("flush")?;

        debug!("X11 capture initialized on root window");

        Ok(Self {
            conn,
            root,
            captured: false,
            grab_window,
        })
    }
}

impl InputCapture for X11Capture {
    fn next_event(&mut self) -> Result<InputEvent> {
        loop {
            let event = self.conn.wait_for_event().context("wait for X11 event")?;

            match event {
                Event::XinputRawMotion(raw) => {
                    // Extract relative motion from axisvalues
                    let (dx, dy) = extract_raw_motion_values(&raw);
                    if dx != 0.0 || dy != 0.0 {
                        return Ok(InputEvent::MouseMove { dx, dy });
                    }
                }
                Event::XinputRawButtonPress(raw) => {
                    let detail = raw.detail;
                    if (4..=7).contains(&detail) {
                        let (sdx, sdy) = match detail {
                            4 => (0.0, -1.0),
                            5 => (0.0, 1.0),
                            6 => (-1.0, 0.0),
                            7 => (1.0, 0.0),
                            _ => continue,
                        };
                        return Ok(InputEvent::Scroll { dx: sdx, dy: sdy });
                    }
                    if let Some(button) = map_x11_button(detail) {
                        return Ok(InputEvent::MouseButton {
                            button,
                            state: ButtonState::Pressed,
                        });
                    }
                }
                Event::XinputRawButtonRelease(raw) => {
                    let detail = raw.detail;
                    if (4..=7).contains(&detail) {
                        continue;
                    }
                    if let Some(button) = map_x11_button(detail) {
                        return Ok(InputEvent::MouseButton {
                            button,
                            state: ButtonState::Released,
                        });
                    }
                }
                Event::XinputRawKeyPress(raw) => {
                    let keycode = raw.detail;
                    return Ok(InputEvent::Key {
                        scancode: keycode.saturating_sub(8), // X11 keycode → USB HID
                        state: KeyState::Pressed,
                    });
                }
                Event::XinputRawKeyRelease(raw) => {
                    let keycode = raw.detail;
                    return Ok(InputEvent::Key {
                        scancode: keycode.saturating_sub(8),
                        state: KeyState::Released,
                    });
                }
                _ => {}
            }
        }
    }

    fn set_capture_active(&mut self, active: bool) -> Result<()> {
        self.captured = active;
        if active {
            self.grab_pointer()?;
        } else {
            self.ungrab_pointer()?;
        }
        Ok(())
    }

    fn grab_pointer(&mut self) -> Result<()> {
        // Map the 1x1 grab window at screen center
        self.conn
            .map_window(self.grab_window)
            .context("map grab window")?;
        self.conn.flush()?;

        // Set input focus to our grab window so key events don't reach other apps
        self.conn
            .set_input_focus(
                xproto::InputFocus::POINTER_ROOT,
                self.grab_window,
                0u32, // CurrentTime
            )
            .context("set input focus")?;

        // GrabPointer event_mask must NOT include keyboard events (X11 protocol rule)
        let event_mask = xproto::EventMask::POINTER_MOTION
            | xproto::EventMask::BUTTON_PRESS
            | xproto::EventMask::BUTTON_RELEASE;
        let reply = self
            .conn
            .grab_pointer(
                true,
                self.grab_window, // grab on our window (must be viewable)
                event_mask,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                self.grab_window, // confine_to: lock cursor in 1x1 window
                0u32,             // no cursor change
                0u32,             // CurrentTime
            )
            .context("grab pointer request")?
            .reply()
            .context("grab pointer reply")?;

        if reply.status != GrabStatus::SUCCESS {
            self.conn.unmap_window(self.grab_window)?;
            self.conn.flush()?;
            anyhow::bail!("failed to grab pointer: {:?}", reply.status);
        }

        self.conn.flush()?;
        debug!("pointer grabbed, focus redirected to grab window");
        Ok(())
    }

    fn ungrab_pointer(&mut self) -> Result<()> {
        self.conn.ungrab_pointer(0u32).context("ungrab pointer")?;
        // Restore focus to root
        self.conn
            .set_input_focus(xproto::InputFocus::POINTER_ROOT, self.root, 0u32)
            .context("restore input focus")?;
        self.conn.unmap_window(self.grab_window)?;
        self.conn.flush()?;
        debug!("pointer ungrabbed, focus restored");
        Ok(())
    }
}

fn extract_raw_motion_values(raw: &xinput::RawMotionEvent) -> (f64, f64) {
    // Use axisvalues_raw (unaccelerated hardware units).
    // The remote compositor will apply its own pointer acceleration profile,
    // making the cursor feel like a locally-connected mouse on the remote screen.
    let mask = &raw.valuator_mask;
    let values = &raw.axisvalues_raw;

    let mut dx = 0.0;
    let mut dy = 0.0;
    let mut value_idx = 0;

    for (mask_word_idx, &mask_word) in mask.iter().enumerate() {
        for bit in 0..32u32 {
            if mask_word & (1 << bit) == 0 {
                continue;
            }
            let axis = mask_word_idx as u32 * 32 + bit;
            if axis == 0 {
                if let Some(v) = values.get(value_idx) {
                    dx = fp3232_to_f64(v);
                }
            } else if axis == 1 {
                if let Some(v) = values.get(value_idx) {
                    dy = fp3232_to_f64(v);
                }
            }
            value_idx += 1;
            if axis > 1 {
                break;
            }
        }
    }
    (dx, dy)
}

fn fp3232_to_f64(v: &xinput::Fp3232) -> f64 {
    v.integral as f64 + (v.frac as f64 / 4294967296.0)
}

fn map_x11_button(button: u32) -> Option<MouseButton> {
    match button {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        8 => Some(MouseButton::Back),
        9 => Some(MouseButton::Forward),
        _ => None,
    }
}
