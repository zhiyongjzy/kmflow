use anyhow::{Context, Result};
use kmflow_proto::{ButtonState, InputEvent, KeyState, MouseButton};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use tracing::{debug, info, warn};

use crate::InputCapture;

const EV_SYN: u16 = 0x00;
const SYN_REPORT: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const EVIOCGRAB: libc::c_ulong = 0x40044590;

#[repr(C)]
struct InputEventRaw {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

const INPUT_EVENT_SIZE: usize = std::mem::size_of::<InputEventRaw>();

pub struct EvdevCapture {
    devices: Vec<EvdevDevice>,
    grabbed: bool,
    held_keys: HashSet<u16>,
    epoll_fd: i32,
    shutdown_fd: i32, // eventfd for clean shutdown
    pending_dx: f64,
    pending_dy: f64,
}

struct EvdevDevice {
    file: File,
    path: String,
}

impl EvdevCapture {
    pub fn new() -> Result<Self> {
        let devices = enumerate_input_devices()?;
        if devices.is_empty() {
            anyhow::bail!(
                "no input devices found (need root or input group for /dev/input/event*)"
            );
        }

        // Create epoll instance and register all device fds
        let epoll_fd = unsafe { libc::epoll_create1(0) };
        if epoll_fd < 0 {
            anyhow::bail!("epoll_create1 failed: {}", std::io::Error::last_os_error());
        }
        for dev in &devices {
            let fd = dev.file.as_raw_fd();
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: fd as u64,
            };
            let ret = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
            if ret < 0 {
                warn!(path = %dev.path, "epoll_ctl ADD failed");
            }
        }

        // Create eventfd for shutdown signaling
        let shutdown_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
        if shutdown_fd < 0 {
            anyhow::bail!("eventfd failed: {}", std::io::Error::last_os_error());
        }
        let mut shutdown_ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: shutdown_fd as u64,
        };
        let ret = unsafe {
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, shutdown_fd, &mut shutdown_ev)
        };
        if ret < 0 {
            warn!("epoll_ctl ADD shutdown_fd failed");
        }

        let names: Vec<&str> = devices.iter().map(|d| d.path.as_str()).collect();
        info!(
            ?names,
            "evdev capture: opened {} input devices (epoll)",
            devices.len()
        );

        Ok(Self {
            devices,
            grabbed: false,
            held_keys: HashSet::new(),
            epoll_fd,
            shutdown_fd,
            pending_dx: 0.0,
            pending_dy: 0.0,
        })
    }
}

impl Drop for EvdevCapture {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.shutdown_fd);
            libc::close(self.epoll_fd);
        }
    }
}

impl InputCapture for EvdevCapture {
    fn next_event(&mut self) -> Result<InputEvent> {
        let mut buf = [0u8; INPUT_EVENT_SIZE];
        let mut epoll_events = [libc::epoll_event { events: 0, u64: 0 }; 16];

        loop {
            let nfds =
                unsafe { libc::epoll_wait(self.epoll_fd, epoll_events.as_mut_ptr(), 16, 100) };

            if nfds < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("epoll_wait failed: {err}");
            }
            if nfds == 0 {
                continue; // timeout, check grab state
            }

            for ev in epoll_events.iter().take(nfds as usize) {
                let ready_fd = ev.u64 as i32;

                // Check if this is the shutdown signal
                if ready_fd == self.shutdown_fd {
                    anyhow::bail!("shutdown");
                }

                // Read all available events from this fd
                loop {
                    let n = unsafe {
                        libc::read(
                            ready_fd,
                            buf.as_mut_ptr() as *mut libc::c_void,
                            INPUT_EVENT_SIZE,
                        )
                    };
                    if n != INPUT_EVENT_SIZE as isize {
                        break; // no more events from this fd
                    }

                    let raw: &InputEventRaw = unsafe { &*(buf.as_ptr() as *const InputEventRaw) };

                    match raw.type_ {
                        EV_SYN
                            if raw.code == SYN_REPORT
                                && (self.pending_dx != 0.0 || self.pending_dy != 0.0) =>
                        {
                            let dx = self.pending_dx;
                            let dy = self.pending_dy;
                            self.pending_dx = 0.0;
                            self.pending_dy = 0.0;
                            return Ok(InputEvent::MouseMove { dx, dy });
                        }
                        EV_REL => {
                            // Accumulate relative motion until SYN_REPORT
                            match raw.code {
                                REL_X => self.pending_dx += raw.value as f64,
                                REL_Y => self.pending_dy += raw.value as f64,
                                REL_WHEEL => {
                                    return Ok(InputEvent::Scroll {
                                        dx: 0.0,
                                        dy: -(raw.value as f64),
                                    });
                                }
                                REL_HWHEEL => {
                                    return Ok(InputEvent::Scroll {
                                        dx: raw.value as f64,
                                        dy: 0.0,
                                    });
                                }
                                _ => {}
                            }
                        }
                        EV_KEY => {
                            if let Some(event) = translate_key_event(raw, &mut self.held_keys) {
                                return Ok(event);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn set_capture_active(&mut self, active: bool) -> Result<()> {
        if active {
            self.grab_pointer()?;
        } else {
            self.ungrab_pointer()?;
        }
        Ok(())
    }

    fn grab_pointer(&mut self) -> Result<()> {
        if self.grabbed {
            return Ok(());
        }
        for dev in &self.devices {
            let fd = dev.file.as_raw_fd();
            let ret = unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_ulong) };
            if ret < 0 {
                warn!(path = %dev.path, "EVIOCGRAB failed (already grabbed by another process?)");
            }
        }
        self.grabbed = true;
        info!("evdev: all devices grabbed");
        Ok(())
    }

    fn ungrab_pointer(&mut self) -> Result<()> {
        if !self.grabbed {
            return Ok(());
        }
        for dev in &self.devices {
            let fd = dev.file.as_raw_fd();
            let _ = unsafe { libc::ioctl(fd, EVIOCGRAB, 0 as libc::c_ulong) };
        }
        self.grabbed = false;
        self.held_keys.clear();
        info!("evdev: all devices ungrabbed");
        Ok(())
    }

    fn shutdown_fd(&self) -> Option<i32> {
        Some(self.shutdown_fd)
    }
}

fn translate_key_event(raw: &InputEventRaw, held_keys: &mut HashSet<u16>) -> Option<InputEvent> {
    let code = raw.code;
    // Mouse buttons
    if (BTN_LEFT..=BTN_EXTRA).contains(&code) {
        let button = match code {
            BTN_LEFT => MouseButton::Left,
            BTN_RIGHT => MouseButton::Right,
            BTN_MIDDLE => MouseButton::Middle,
            BTN_SIDE => MouseButton::Back,
            BTN_EXTRA => MouseButton::Forward,
            _ => return None,
        };
        let state = if raw.value != 0 {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        Some(InputEvent::MouseButton { button, state })
    } else if raw.value == 2 {
        // autorepeat — ignore
        None
    } else {
        // Keyboard key (evdev code = linux scancode)
        let state = if raw.value != 0 {
            held_keys.insert(code);
            KeyState::Pressed
        } else {
            held_keys.remove(&code);
            KeyState::Released
        };
        Some(InputEvent::Key {
            scancode: code as u32,
            state,
        })
    }
}

fn enumerate_input_devices() -> Result<Vec<EvdevDevice>> {
    let mut devices = Vec::new();

    let entries = fs::read_dir("/dev/input").context("read /dev/input")?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        if !name.starts_with("event") {
            continue;
        }

        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => {
                // Set non-blocking for epoll-based reading
                unsafe {
                    libc::fcntl(f.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
                }
                f
            }
            Err(_) => continue, // no permission, skip
        };

        let fd = file.as_raw_fd();

        // Check capabilities to see if this is a keyboard or mouse
        let has_keys = has_event_type(fd, EV_KEY);
        let has_rel = has_event_type(fd, EV_REL);

        // Only grab devices that are actually keyboards or mice
        if !has_keys && !has_rel {
            continue;
        }

        // Filter out non-physical devices (power buttons, video bus, etc.)
        // by checking if it has either relative axes (mouse) or a reasonable key range
        let is_mouse = has_rel && has_key(fd, BTN_LEFT);
        let is_keyboard = has_keys && has_key(fd, 30); // KEY_A = 30

        if !is_mouse && !is_keyboard {
            continue;
        }

        // Skip our own virtual device
        let dev_name = get_device_name(fd);
        if dev_name.contains("KMFlow") {
            debug!(path = %path.display(), "skipping our own virtual device");
            continue;
        }

        debug!(path = %path.display(), name = %dev_name, is_mouse, is_keyboard, "found input device");

        devices.push(EvdevDevice {
            file,
            path: path.to_string_lossy().to_string(),
        });
    }

    Ok(devices)
}

fn has_event_type(fd: i32, ev_type: u16) -> bool {
    // EVIOCGBIT(0, size) gets the event type bitmap
    let mut bits = [0u8; 4]; // EV_MAX is ~0x1f, 4 bytes is enough
    let request: libc::c_ulong = 0x80044520; // EVIOCGBIT(0, 4)
    let ret = unsafe { libc::ioctl(fd, request, bits.as_mut_ptr()) };
    if ret < 0 {
        return false;
    }
    let byte_idx = ev_type as usize / 8;
    let bit_idx = ev_type as usize % 8;
    byte_idx < bits.len() && (bits[byte_idx] & (1 << bit_idx)) != 0
}

fn has_key(fd: i32, key: u16) -> bool {
    // EVIOCGBIT(EV_KEY, size) gets the key bitmap
    let mut bits = [0u8; 96]; // KEY_MAX is ~767, need 96 bytes
    // EVIOCGBIT(EV_KEY, 96) = _IOC(_IOC_READ, 'E', 0x20 + EV_KEY, 96)
    let request: libc::c_ulong = 0x80604521; // EVIOCGBIT(1, 96)
    let ret = unsafe { libc::ioctl(fd, request, bits.as_mut_ptr()) };
    if ret < 0 {
        return false;
    }
    let byte_idx = key as usize / 8;
    let bit_idx = key as usize % 8;
    byte_idx < bits.len() && (bits[byte_idx] & (1 << bit_idx)) != 0
}

fn get_device_name(fd: i32) -> String {
    let mut name_buf = [0u8; 256];
    // EVIOCGNAME(256)
    let request: libc::c_ulong = 0x81004506;
    let ret = unsafe { libc::ioctl(fd, request, name_buf.as_mut_ptr()) };
    if ret < 0 {
        return String::new();
    }
    let len = name_buf.iter().position(|&b| b == 0).unwrap_or(256);
    String::from_utf8_lossy(&name_buf[..len]).to_string()
}
