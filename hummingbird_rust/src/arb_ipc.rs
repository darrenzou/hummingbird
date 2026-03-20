use anyhow::Context;
use serde::{de::DeserializeOwned, Serialize};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::io::RawFd;

pub fn pipe_pair() -> anyhow::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe failed");
    }
    Ok((fds[0], fds[1]))
}

pub fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

fn write_full(mut w: impl Write, buf: &[u8]) -> anyhow::Result<()> {
    let mut off = 0usize;
    while off < buf.len() {
        let n = w.write(&buf[off..])?;
        if n == 0 {
            anyhow::bail!("write returned 0");
        }
        off += n;
    }
    Ok(())
}

fn read_full(mut r: impl Read, buf: &mut [u8]) -> anyhow::Result<()> {
    let mut off = 0usize;
    while off < buf.len() {
        let n = r.read(&mut buf[off..])?;
        if n == 0 {
            anyhow::bail!("EOF");
        }
        off += n;
    }
    Ok(())
}

pub fn send_msg<T: Serialize>(fd: RawFd, msg: &T) -> anyhow::Result<()> {
    let bytes = bincode::serialize(msg).context("bincode serialize")?;
    let len = bytes.len() as u32;
    let header = len.to_le_bytes();
    let dup = unsafe { libc::dup(fd) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("dup fd for send");
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(dup) };
    write_full(&mut file, &header)?;
    write_full(&mut file, &bytes)?;
    Ok(())
}

pub fn recv_msg<T: DeserializeOwned>(fd: RawFd) -> anyhow::Result<T> {
    let mut header = [0u8; 4];
    let dup = unsafe { libc::dup(fd) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("dup fd for recv");
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(dup) };
    read_full(&mut file, &mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len > 8 * 1024 * 1024 {
        anyhow::bail!("ipc message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    read_full(&mut file, &mut buf)?;
    let msg = bincode::deserialize::<T>(&buf).context("bincode deserialize")?;
    Ok(msg)
}

pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> anyhow::Result<bool> {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE as usize {
        anyhow::bail!("invalid fd for select");
    }
    let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
    unsafe { libc::FD_ZERO(&mut rfds) };
    unsafe { libc::FD_SET(fd, &mut rfds) };

    let mut tv = libc::timeval {
        tv_sec: (timeout_ms / 1000) as libc::time_t,
        tv_usec: ((timeout_ms % 1000) * 1000) as libc::suseconds_t,
    };
    let r = unsafe { libc::select(fd + 1, &mut rfds, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) };
    if r < 0 {
        return Err(std::io::Error::last_os_error()).context("select failed");
    }
    Ok(r > 0 && unsafe { libc::FD_ISSET(fd, &mut rfds) })
}

