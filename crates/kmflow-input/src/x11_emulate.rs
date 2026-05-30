use anyhow::{Context, Result};
use kmflow_proto::{ButtonState, InputEvent, KeyState, MouseButton};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::InputEmulator;

pub struct X11Emulator {
    conn: RustConnection,
    #[allow(dead_code)]
    screen_num: usize,
}

impl X11Emulator {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None).context("connect to X11")?;

        let xtest = conn
            .query_extension(b"XTEST")
            .context("query xtest")?
            .reply()
            .context("xtest reply")?;
        if !xtest.present {
            anyhow::bail!("XTest extension not available");
        }

        tracing::debug!("X11 emulator initialized");
        Ok(Self { conn, screen_num })
    }
}

impl InputEmulator for X11Emulator {
    fn emit(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                x11rb::protocol::xtest::fake_input(
                    &self.conn,
                    6, // MotionNotify relative
                    0,
                    0,
                    x11rb::NONE,
                    *dx as i16,
                    *dy as i16,
                    0, // deviceid
                )
                .context("fake relative motion")?;
            }
            InputEvent::MouseButton { button, state } => {
                let x11_button = match button {
                    MouseButton::Left => 1u8,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 9,
                };
                let event_type = match state {
                    ButtonState::Pressed => 4u8,  // ButtonPress
                    ButtonState::Released => 5u8, // ButtonRelease
                };
                x11rb::protocol::xtest::fake_input(
                    &self.conn,
                    event_type,
                    x11_button,
                    0,
                    x11rb::NONE,
                    0,
                    0,
                    0, // deviceid
                )
                .context("fake button")?;
            }
            InputEvent::Scroll { dx, dy } => {
                if *dy < 0.0 {
                    for _ in 0..(-*dy as i32).max(1) {
                        fake_button_click(&self.conn, 4)?;
                    }
                } else if *dy > 0.0 {
                    for _ in 0..(*dy as i32).max(1) {
                        fake_button_click(&self.conn, 5)?;
                    }
                }
                if *dx < 0.0 {
                    for _ in 0..(-*dx as i32).max(1) {
                        fake_button_click(&self.conn, 6)?;
                    }
                } else if *dx > 0.0 {
                    for _ in 0..(*dx as i32).max(1) {
                        fake_button_click(&self.conn, 7)?;
                    }
                }
            }
            InputEvent::Key { scancode, state } => {
                let keycode = scancode + 8; // HID to X11 keycode offset
                let event_type = match state {
                    KeyState::Pressed => 2u8,  // KeyPress
                    KeyState::Released => 3u8, // KeyRelease
                };
                x11rb::protocol::xtest::fake_input(
                    &self.conn,
                    event_type,
                    keycode as u8,
                    0,
                    x11rb::NONE,
                    0,
                    0,
                    0, // deviceid
                )
                .context("fake key")?;
            }
        }
        self.conn.flush().context("flush X11")?;
        Ok(())
    }
}

fn fake_button_click(conn: &RustConnection, button: u8) -> Result<()> {
    x11rb::protocol::xtest::fake_input(conn, 4, button, 0, x11rb::NONE, 0, 0, 0)?;
    x11rb::protocol::xtest::fake_input(conn, 5, button, 0, x11rb::NONE, 0, 0, 0)?;
    Ok(())
}
