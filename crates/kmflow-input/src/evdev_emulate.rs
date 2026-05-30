use anyhow::{Context, Result};
use kmflow_proto::{ButtonState, InputEvent, KeyState, MouseButton};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use tracing::info;

use crate::InputEmulator;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const SYN_REPORT: u16 = 0x00;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const UI_DEV_SETUP: u64 = 0x405c5503;
const UI_DEV_CREATE: u64 = 0x5501;
const UI_SET_EVBIT: u64 = 0x40045564;
const UI_SET_KEYBIT: u64 = 0x40045565;
const UI_SET_RELBIT: u64 = 0x40045566;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C, packed)]
struct InputEventRaw {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

pub struct EvdevEmulator {
    uinput_file: File,
}

impl EvdevEmulator {
    pub fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("open /dev/uinput (need root or uinput group)")?;

        let fd = file.as_raw_fd();

        unsafe {
            ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_ulong)?;
            ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_ulong)?;
            ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_ulong)?;

            ioctl(fd, UI_SET_RELBIT, REL_X as libc::c_ulong)?;
            ioctl(fd, UI_SET_RELBIT, REL_Y as libc::c_ulong)?;
            ioctl(fd, UI_SET_RELBIT, REL_WHEEL as libc::c_ulong)?;
            ioctl(fd, UI_SET_RELBIT, REL_HWHEEL as libc::c_ulong)?;

            ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as libc::c_ulong)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT as libc::c_ulong)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_MIDDLE as libc::c_ulong)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_SIDE as libc::c_ulong)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_EXTRA as libc::c_ulong)?;

            for key in 1u16..256 {
                ioctl(fd, UI_SET_KEYBIT, key as libc::c_ulong)?;
            }

            let mut setup = UinputSetup {
                id: InputId {
                    bustype: 0x06, // BUS_VIRTUAL
                    vendor: 0x1234,
                    product: 0x5678,
                    version: 1,
                },
                name: [0u8; 80],
                ff_effects_max: 0,
            };
            let name = b"KMFlow Virtual Input";
            setup.name[..name.len()].copy_from_slice(name);

            let ret = libc::ioctl(fd, UI_DEV_SETUP, &setup);
            if ret < 0 {
                anyhow::bail!("UI_DEV_SETUP failed: {}", std::io::Error::last_os_error());
            }

            let ret = libc::ioctl(fd, UI_DEV_CREATE);
            if ret < 0 {
                anyhow::bail!("UI_DEV_CREATE failed: {}", std::io::Error::last_os_error());
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        info!("uinput virtual device created: KMFlow Virtual Input");
        Ok(Self { uinput_file: file })
    }

    fn write_event(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
        let event = InputEventRaw {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &event as *const InputEventRaw as *const u8,
                std::mem::size_of::<InputEventRaw>(),
            )
        };
        self.uinput_file
            .write_all(bytes)
            .context("write uinput event")?;
        Ok(())
    }

    fn syn_report(&mut self) -> Result<()> {
        self.write_event(EV_SYN, SYN_REPORT, 0)
    }
}

unsafe fn ioctl(fd: i32, request: u64, value: libc::c_ulong) -> Result<()> {
    let ret = unsafe { libc::ioctl(fd, request, value) };
    if ret < 0 {
        anyhow::bail!(
            "ioctl 0x{:x} failed: {}",
            request,
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

impl InputEmulator for EvdevEmulator {
    fn emit(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                if *dx != 0.0 {
                    self.write_event(EV_REL, REL_X, *dx as i32)?;
                }
                if *dy != 0.0 {
                    self.write_event(EV_REL, REL_Y, *dy as i32)?;
                }
                self.syn_report()?;
            }
            InputEvent::MouseButton { button, state } => {
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_RIGHT,
                    MouseButton::Middle => BTN_MIDDLE,
                    MouseButton::Back => BTN_SIDE,
                    MouseButton::Forward => BTN_EXTRA,
                };
                let value = match state {
                    ButtonState::Pressed => 1,
                    ButtonState::Released => 0,
                };
                self.write_event(EV_KEY, code, value)?;
                self.syn_report()?;
            }
            InputEvent::Scroll { dx, dy } => {
                if *dy != 0.0 {
                    self.write_event(EV_REL, REL_WHEEL, -(*dy as i32))?;
                }
                if *dx != 0.0 {
                    self.write_event(EV_REL, REL_HWHEEL, *dx as i32)?;
                }
                self.syn_report()?;
            }
            InputEvent::Key { scancode, state } => {
                let value = match state {
                    KeyState::Pressed => 1,
                    KeyState::Released => 0,
                };
                self.write_event(EV_KEY, *scancode as u16, value)?;
                self.syn_report()?;
            }
        }
        Ok(())
    }
}
