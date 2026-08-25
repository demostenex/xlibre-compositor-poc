use std::error::Error;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

use x11rb::connection::Connection;
use x11rb::protocol::Event;

use super::connection::X11Connection;

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

unsafe extern "C" fn signal_handler(_signal: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        // EAGAIN means a wake is already pending; never block in the handler.
        let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
    }
}

#[derive(Debug)]
pub enum WaitResult {
    Event(Event),
    Shutdown,
}

pub struct SignalWake {
    read: UnixStream,
    _write: UnixStream,
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
}

impl SignalWake {
    pub fn install() -> Result<Self, Box<dyn Error>> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        let write_fd = write.as_raw_fd();
        if SIGNAL_WRITE_FD
            .compare_exchange(-1, write_fd, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("signal shutdown is already installed".into());
        }

        let action = unsafe { make_action() };
        let mut previous_int = unsafe { std::mem::zeroed() };
        if let Err(error) = unsafe {
            install_signal(libc::SIGINT, &action, &mut previous_int)
        } {
            SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
            return Err(error.into());
        }
        let mut previous_term = unsafe { std::mem::zeroed() };
        if let Err(error) = unsafe {
            install_signal(libc::SIGTERM, &action, &mut previous_term)
        } {
            unsafe {
                let _ = libc::sigaction(libc::SIGINT, &previous_int, std::ptr::null_mut());
            }
            SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
            return Err(error.into());
        }

        Ok(Self {
            read,
            _write: write,
            previous_int,
            previous_term,
        })
    }

    pub fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    pub fn drain(&mut self) -> Result<(), Box<dyn Error>> {
        let mut buffer = [0u8; 64];
        loop {
            match self.read.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub fn poll_shutdown_pending(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut descriptor = libc::pollfd {
            fd: self.read_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(error.into());
            }
            if descriptor.revents & libc::POLLNVAL != 0 {
                return Err("shutdown wake descriptor became invalid".into());
            }
            if !shutdown_ready(descriptor.revents) {
                return Ok(false);
            }
            self.drain()?;
            return Ok(true);
        }
    }
}

impl Drop for SignalWake {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
            let _ = libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
        }
        SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
    }
}

pub fn wait_for_event_or_shutdown(
    connection: &X11Connection,
    signal: &mut SignalWake,
) -> Result<WaitResult, Box<dyn Error>> {
    loop {
        if let Some(event) = connection.inner.poll_for_event()? {
            return Ok(WaitResult::Event(event));
        }

        let x_fd = connection.inner.as_raw_fd();
        if x_fd < 0 {
            return Err("failed to obtain X11 connection file descriptor".into());
        }
        let mut descriptors = [
            libc::pollfd {
                fd: signal.read_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: x_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error.into());
        }
        if descriptors[0].revents & libc::POLLNVAL != 0 {
            return Err("shutdown wake descriptor became invalid".into());
        }
        if shutdown_ready(descriptors[0].revents) {
            signal.drain()?;
            return Ok(WaitResult::Shutdown);
        }
        if descriptors[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err("X11 connection became unavailable".into());
        }
    }
}

fn shutdown_ready(revents: libc::c_short) -> bool {
    revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

unsafe fn install_signal(
    signal: libc::c_int,
    action: &libc::sigaction,
    previous: &mut libc::sigaction,
) -> Result<(), std::io::Error> {
    if unsafe { libc::sigaction(signal, action, previous) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

unsafe fn make_action() -> libc::sigaction {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = signal_handler as *const () as usize;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    action.sa_flags = 0;
    action
}
