use anyhow::Result;
use kmflow_proto::InputEvent;

#[cfg(feature = "x11")]
pub mod x11_capture;
#[cfg(feature = "x11")]
pub mod x11_emulate;

#[cfg(feature = "evdev")]
pub mod evdev_capture;
#[cfg(feature = "evdev")]
pub mod evdev_emulate;

pub trait InputCapture: Send {
    fn next_event(&mut self) -> Result<InputEvent>;
    fn set_capture_active(&mut self, active: bool) -> Result<()>;
    fn grab_pointer(&mut self) -> Result<()>;
    fn ungrab_pointer(&mut self) -> Result<()>;
    /// Returns a file descriptor that, when written to, interrupts next_event().
    fn shutdown_fd(&self) -> Option<i32> {
        None
    }
}

pub trait InputEmulator: Send {
    fn emit(&mut self, event: &InputEvent) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    X11,
    Evdev,
}

pub fn detect_backend() -> Backend {
    // If DISPLAY is set to a non-empty value and accessible, use X11
    // Otherwise use evdev (works everywhere with root/input group)
    match std::env::var("DISPLAY") {
        Ok(val) if !val.is_empty() => Backend::X11,
        _ => Backend::Evdev,
    }
}

pub fn create_capture(backend: Backend) -> Result<Box<dyn InputCapture>> {
    match backend {
        #[cfg(feature = "x11")]
        Backend::X11 => match x11_capture::X11Capture::new() {
            Ok(c) => Ok(Box::new(c)),
            Err(e) => {
                tracing::warn!("X11 capture failed ({e:#}), trying evdev fallback");
                #[cfg(feature = "evdev")]
                {
                    Ok(Box::new(evdev_capture::EvdevCapture::new()?))
                }
                #[cfg(not(feature = "evdev"))]
                Err(e)
            }
        },
        #[cfg(feature = "evdev")]
        Backend::Evdev => Ok(Box::new(evdev_capture::EvdevCapture::new()?)),
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("backend {backend:?} not compiled in"),
    }
}

pub fn create_emulator(backend: Backend) -> Result<Box<dyn InputEmulator>> {
    match backend {
        #[cfg(feature = "x11")]
        Backend::X11 => match x11_emulate::X11Emulator::new() {
            Ok(e) => Ok(Box::new(e)),
            Err(x11_err) => {
                #[cfg(feature = "evdev")]
                {
                    tracing::warn!("X11 emulator failed ({x11_err:#}), trying uinput fallback");
                    Ok(Box::new(evdev_emulate::EvdevEmulator::new()?))
                }
                #[cfg(not(feature = "evdev"))]
                Err(x11_err)
            }
        },
        #[cfg(feature = "evdev")]
        Backend::Evdev => Ok(Box::new(evdev_emulate::EvdevEmulator::new()?)),
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("backend {backend:?} not compiled in"),
    }
}
