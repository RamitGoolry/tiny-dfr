use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt, unix::io::OwnedFd},
    path::Path,
};

use ::input::LibinputInterface;
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, timeval};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};

pub(crate) struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            // libinput wants an errno; fall back to EIO for the rare non-OS error.
            .map_err(|err| err.raw_os_error().unwrap_or(libc::EIO))
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    // Fires on every key edge: a transient write failure drops the event but must
    // not bring the daemon down.
    if let Err(e) = uinput.write(&[input_event {
        value,
        type_: ty as u16,
        code,
        time: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    }]) {
        eprintln!(
            "failed to emit input event (type={}, code={code}, value={value}): {e}",
            ty as u16
        );
    }
}

pub(crate) fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<Key>, value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for kc in codes {
        emit(uinput, EventKind::Key, *kc as u16, value);
    }
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}
