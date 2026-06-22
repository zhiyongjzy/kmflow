use anyhow::{Context, Result};
use kmflow_proto::{ButtonState, InputEvent, KeyState, MouseButton};
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, GrabMode, GrabStatus};
use x11rb::rust_connection::RustConnection;

use crate::InputCapture;

const EVIOCGRAB: libc::c_ulong = 0x40044590;
const EV_KEY: u16 = 0x01;

#[repr(C)]
struct EvdevEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

const EVDEV_EVENT_SIZE: usize = std::mem::size_of::<EvdevEvent>();

pub struct X11Capture {
    conn: RustConnection,
    root: xproto::Window,
    captured: bool,
    grab_window: xproto::Window,
    keyboard_devices: Vec<File>,
    kbd_grabbed: bool,
    epoll_fd: i32,
    x11_fd: i32,
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

        // Get X11 connection fd for epoll
        let x11_fd = conn.stream().as_raw_fd();

        // Find keyboard devices for evdev grab
        let keyboard_devices = find_keyboard_devices();
        if keyboard_devices.is_empty() {
            warn!("no keyboard devices found for evdev grab, compositor shortcuts may leak");
        } else {
            info!("found {} keyboard device(s) for evdev grab", keyboard_devices.len());
        }

        // Create epoll instance
        let epoll_fd = unsafe { libc::epoll_create1(0) };
        if epoll_fd < 0 {
            anyhow::bail!("epoll_create1 failed: {}", std::io::Error::last_os_error());
        }

        // Register X11 fd
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: x11_fd as u64,
        };
        unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, x11_fd, &mut ev) };

        // Register keyboard device fds
        for dev in &keyboard_devices {
            let fd = dev.as_raw_fd();
            // Set non-blocking
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: fd as u64,
            };
            unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        }

        debug!("X11 capture initialized on root window");

        Ok(Self {
            conn,
            root,
            captured: false,
            grab_window,
            keyboard_devices,
            kbd_grabbed: false,
            epoll_fd,
            x11_fd,
        })
    }

    /// Try to read a keyboard event from evdev fds (non-blocking)
    fn try_read_evdev_key(&self) -> Option<InputEvent> {
        let mut buf = [0u8; EVDEV_EVENT_SIZE];
        for dev in &self.keyboard_devices {
            let fd = dev.as_raw_fd();
            loop {
                let n = unsafe {
                    libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, EVDEV_EVENT_SIZE)
                };
                if n != EVDEV_EVENT_SIZE as isize {
                    break;
                }
                let raw: &EvdevEvent = unsafe { &*(buf.as_ptr() as *const EvdevEvent) };
                if raw.type_ == EV_KEY && raw.value != 2 {
                    // value: 0=released, 1=pressed, 2=autorepeat (skip)
                    let state = if raw.value != 0 {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    return Some(InputEvent::Key {
                        scancode: raw.code as u32,
                        state,
                    });
                }
            }
        }
        None
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        // Release any evdev grabs
        if self.kbd_grabbed {
            for dev in &self.keyboard_devices {
                let fd = dev.as_raw_fd();
                let _ = unsafe { libc::ioctl(fd, EVIOCGRAB, 0 as libc::c_ulong) };
            }
        }
        unsafe { libc::close(self.epoll_fd) };
    }
}

impl InputCapture for X11Capture {
    fn next_event(&mut self) -> Result<InputEvent> {
        loop {
            // When keyboard is grabbed, check evdev fds first for any pending key events
            if self.kbd_grabbed {
                if let Some(ev) = self.try_read_evdev_key() {
                    return Ok(ev);
                }
            }

            // Poll with epoll — wait for X11 events or evdev keyboard events
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 8];
            let timeout = if self.kbd_grabbed { 10 } else { -1 }; // 10ms poll when grabbed
            let nfds = unsafe { libc::epoll_wait(self.epoll_fd, events.as_mut_ptr(), 8, timeout) };

            if nfds < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("epoll_wait failed: {err}");
            }

            // Check if any evdev keyboard has data (when grabbed)
            if self.kbd_grabbed {
                for ep_ev in events.iter().take(nfds as usize) {
                    let ready_fd = ep_ev.u64 as i32;
                    if ready_fd != self.x11_fd {
                        if let Some(ev) = self.try_read_evdev_key() {
                            return Ok(ev);
                        }
                    }
                }
            }

            // Process X11 events (mouse + keyboard when not grabbed)
            while let Some(event) = self.conn.poll_for_event().context("poll X11 event")? {
                match event {
                    Event::XinputRawMotion(raw) => {
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
                        // Only process XI2 key events when keyboard is NOT evdev-grabbed
                        if !self.kbd_grabbed {
                            let keycode = raw.detail;
                            return Ok(InputEvent::Key {
                                scancode: keycode.saturating_sub(8),
                                state: KeyState::Pressed,
                            });
                        }
                    }
                    Event::XinputRawKeyRelease(raw) => {
                        if !self.kbd_grabbed {
                            let keycode = raw.detail;
                            return Ok(InputEvent::Key {
                                scancode: keycode.saturating_sub(8),
                                state: KeyState::Released,
                            });
                        }
                    }
                    _ => {}
                }
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

        // GrabPointer: confine cursor to 1x1 window
        let event_mask = xproto::EventMask::POINTER_MOTION
            | xproto::EventMask::BUTTON_PRESS
            | xproto::EventMask::BUTTON_RELEASE;
        let reply = self
            .conn
            .grab_pointer(
                true,
                self.grab_window,
                event_mask,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                self.grab_window, // confine_to
                0u32,
                0u32,
            )
            .context("grab pointer request")?
            .reply()
            .context("grab pointer reply")?;

        if reply.status != GrabStatus::SUCCESS {
            self.conn.unmap_window(self.grab_window)?;
            self.conn.flush()?;
            anyhow::bail!("failed to grab pointer: {:?}", reply.status);
        }

        // Grab keyboard at evdev level — prevents compositor from seeing key events
        let mut grabbed_count = 0;
        for dev in &self.keyboard_devices {
            let fd = dev.as_raw_fd();
            let ret = unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_ulong) };
            if ret == 0 {
                grabbed_count += 1;
            } else {
                warn!("EVIOCGRAB failed on keyboard fd={}", fd);
            }
        }
        if grabbed_count > 0 {
            self.kbd_grabbed = true;
            info!("evdev keyboard grab: {} device(s) grabbed", grabbed_count);
        }

        self.conn.flush()?;
        Ok(())
    }

    fn ungrab_pointer(&mut self) -> Result<()> {
        // Release evdev keyboard grab
        if self.kbd_grabbed {
            for dev in &self.keyboard_devices {
                let fd = dev.as_raw_fd();
                let _ = unsafe { libc::ioctl(fd, EVIOCGRAB, 0 as libc::c_ulong) };
            }
            self.kbd_grabbed = false;
        }

        self.conn.ungrab_pointer(0u32).context("ungrab pointer")?;
        self.conn
            .set_input_focus(xproto::InputFocus::POINTER_ROOT, self.root, 0u32)
            .context("restore input focus")?;
        self.conn.unmap_window(self.grab_window)?;
        self.conn.flush()?;
        debug!("pointer and keyboard ungrabbed, focus restored");
        Ok(())
    }
}

fn extract_raw_motion_values(raw: &xinput::RawMotionEvent) -> (f64, f64) {
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

/// Find evdev keyboard devices (devices that have KEY_A but not REL_X)
fn find_keyboard_devices() -> Vec<File> {
    let mut devices = Vec::new();
    let entries = match fs::read_dir("/dev/input") {
        Ok(e) => e,
        Err(_) => return devices,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if !name.starts_with("event") {
            continue;
        }

        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let fd = file.as_raw_fd();

        // Check if device has EV_KEY capability
        let mut bits = [0u8; 4];
        let request: libc::c_ulong = 0x80044520; // EVIOCGBIT(0, 4)
        let ret = unsafe { libc::ioctl(fd, request, bits.as_mut_ptr()) };
        if ret < 0 {
            continue;
        }
        if bits[0] & (1 << 1) == 0 {
            continue;
        }

        // Check if device has KEY_A (scancode 30)
        let mut key_bits = [0u8; 96];
        let key_request: libc::c_ulong = 0x80604521; // EVIOCGBIT(EV_KEY, 96)
        let ret = unsafe { libc::ioctl(fd, key_request, key_bits.as_mut_ptr()) };
        if ret < 0 {
            continue;
        }
        if key_bits[30 / 8] & (1 << (30 % 8)) == 0 {
            continue;
        }

        // Skip mice (have REL_X)
        let mut rel_bits = [0u8; 4];
        let rel_request: libc::c_ulong = 0x80044522; // EVIOCGBIT(EV_REL, 4)
        let ret = unsafe { libc::ioctl(fd, rel_request, rel_bits.as_mut_ptr()) };
        if ret >= 0 && rel_bits[0] & 1 != 0 {
            continue;
        }

        // Get device name, skip KMFlow virtual device
        let mut name_buf = [0u8; 256];
        let name_request: libc::c_ulong = 0x81004506; // EVIOCGNAME(256)
        let ret = unsafe { libc::ioctl(fd, name_request, name_buf.as_mut_ptr()) };
        if ret > 0 {
            let dev_name = String::from_utf8_lossy(&name_buf[..ret as usize]);
            if dev_name.contains("KMFlow") {
                continue;
            }
            debug!("found keyboard device: {} ({})", path.display(), dev_name.trim_end_matches('\0'));
        }

        devices.push(file);
    }

    devices
}
