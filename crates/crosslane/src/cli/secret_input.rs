//! Shared hidden terminal input with cancellation and echo restoration.
use anyhow::{anyhow, Context, Result};

#[cfg(unix)]
struct HiddenTerminal {
    fd: std::os::fd::RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl HiddenTerminal {
    fn new(fd: std::os::fd::RawFd) -> Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the live output pointer on success.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("读取终端模式失败");
        }
        // SAFETY: the successful tcgetattr call initialized this value.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        // Read Ctrl-C ourselves, so every normal cancellation restores echo.
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        // SAFETY: fd is a live terminal and hidden is fully initialized.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(std::io::Error::last_os_error()).context("关闭终端输入回显失败");
        }
        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for HiddenTerminal {
    fn drop(&mut self) {
        // SAFETY: the guard is dropped before its borrowed terminal is closed.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
pub(crate) fn prompt_hidden_line(
    tty: &mut std::fs::File,
    prompt: &str,
    allow_empty: bool,
) -> Result<String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    // Hide input before advertising readiness to receive a secret. Disabling
    // echo after printing the prompt leaves a race with fast typing/pasting.
    let _guard = HiddenTerminal::new(tty.as_raw_fd())?;
    write!(tty, "{prompt}")?;
    tty.flush()?;
    let result = read_hidden_line(tty, allow_empty);
    writeln!(tty)?;
    result
}

#[cfg(all(test, unix))]
fn hidden_line(tty: &mut std::fs::File, allow_empty: bool) -> Result<String> {
    use std::os::fd::AsRawFd;
    let _guard = HiddenTerminal::new(tty.as_raw_fd())?;
    read_hidden_line(tty, allow_empty)
}

#[cfg(unix)]
fn read_hidden_line(tty: &mut std::fs::File, allow_empty: bool) -> Result<String> {
    use std::io::Read;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    loop {
        let mut byte = [0u8];
        if tty.read(&mut byte)? == 0 {
            return Err(anyhow!("终端输入已结束"));
        }
        match byte[0] {
            b'\r' | b'\n' => break,
            3 | 4 | 27 => return Err(anyhow!("隐藏输入已取消")),
            8 | 127 => {
                if bytes.pop().is_some_and(|last| last & 0xc0 == 0x80) {
                    while bytes.last().is_some_and(|last| last & 0xc0 == 0x80) {
                        bytes.pop();
                    }
                    bytes.pop();
                }
            }
            21 => bytes.clear(),
            value if value >= 32 => bytes.push(value),
            _ => {}
        }
        if bytes.len() > 16384 {
            return Err(anyhow!("隐藏输入过长"));
        }
    }
    if bytes.is_empty() && !allow_empty {
        return Err(anyhow!("隐藏输入不能为空"));
    }
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|_| anyhow!("隐藏输入必须是有效 UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    fn terminal_mode(fd: std::os::fd::RawFd) -> libc::termios {
        let mut mode = std::mem::MaybeUninit::uninit();
        // SAFETY: fd is owned by the test; tcgetattr initializes mode on success.
        assert_eq!(unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) }, 0);
        // SAFETY: tcgetattr succeeded.
        unsafe { mode.assume_init() }
    }

    #[cfg(unix)]
    #[test]
    fn hidden_password_input_does_not_echo_and_restores_terminal_after_cancel() {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::time::{Duration, Instant};

        for (input, expected, allow_empty) in [
            ("  pass密\u{7f}word  \n", Some("  password  "), false),
            ("discard\u{15}replacement\n", Some("replacement"), false),
            ("private\u{3}", None, false),
            ("private\u{4}", None, false),
            ("\n", None, false),
            ("\n", Some(""), true),
        ] {
            let (mut master_fd, mut slave_fd) = (-1, -1);
            // SAFETY: output pointers are valid; optional attributes/name are null.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master_fd,
                        &mut slave_fd,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // SAFETY: openpty returned two new, independently owned descriptors.
            let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
            let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
            let original = terminal_mode(slave.as_raw_fd());
            let mut reader = slave.try_clone().unwrap();
            let task = std::thread::spawn(move || hidden_line(&mut reader, allow_empty));
            let deadline = Instant::now() + Duration::from_secs(2);
            while terminal_mode(slave.as_raw_fd()).c_lflag & libc::ECHO != 0 {
                assert!(
                    Instant::now() < deadline,
                    "password reader did not disable echo"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            master.write_all(input.as_bytes()).unwrap();
            while !task.is_finished() {
                assert!(Instant::now() < deadline, "password reader did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
            let result = task.join().unwrap();
            match expected {
                Some(value) => assert_eq!(result.unwrap(), value),
                None => assert!(result.is_err()),
            }
            // BSD may set internal pending-input flags when restoring canonical mode.
            let restored = terminal_mode(slave.as_raw_fd());
            let changed = libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG;
            assert_eq!(restored.c_lflag & changed, original.c_lflag & changed);
            assert_eq!(restored.c_cc, original.c_cc);
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives a valid single-element array and does not retain it.
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 0) },
                0,
                "password was echoed"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn secret_prompt_is_visible_only_after_echo_is_disabled() {
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        let (mut master_fd, mut slave_fd) = (-1, -1);
        // SAFETY: the output pointers are valid and optional inputs are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned independently owned descriptors.
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
        let original = terminal_mode(slave.as_raw_fd());
        let mut reader = slave.try_clone().unwrap();
        let task = std::thread::spawn(move || prompt_hidden_line(&mut reader, "Key: ", false));
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives a valid one-element array.
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 2000) }, 1);
        let mut prompt = [0u8; 5];
        master.read_exact(&mut prompt).unwrap();
        assert_eq!(&prompt, b"Key: ");
        assert_eq!(terminal_mode(slave.as_raw_fd()).c_lflag & libc::ECHO, 0);
        master.write_all(b"fixture-secret\n").unwrap();
        assert_eq!(task.join().unwrap().unwrap(), "fixture-secret");
        assert_eq!(
            terminal_mode(slave.as_raw_fd()).c_lflag & libc::ECHO,
            original.c_lflag & libc::ECHO
        );
        let mut trailing = [0u8; 64];
        let count = master.read(&mut trailing).unwrap();
        assert!(trailing[..count]
            .iter()
            .all(|byte| matches!(byte, b'\r' | b'\n')));
    }
}
