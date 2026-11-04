//! The SP-404MKII's serial port on Windows, with overlapped I/O.
//!
//! Overlapped I/O lets a read wait on the port while writes go out, so the reader can
//! block indefinitely instead of spinning on a short timeout (synchronous reads and
//! writes on one handle queue behind each other). When the device is removed, the
//! pending read fails at once.

use std::io;
use std::sync::Mutex;

use windows::Win32::Devices::Communication::{
    COMMTIMEOUTS, DCB, GetCommState, NOPARITY, ONESTOPBIT, PURGE_RXCLEAR, PurgeComm, SetCommState,
    SetCommTimeouts,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DIREG_DEV, SP_DEVINFO_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Ioctl::GUID_DEVINTERFACE_COMPORT;
use windows::Win32::System::Registry::{KEY_READ, RegCloseKey, RegQueryValueExW};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{HSTRING, PCWSTR, w};

use super::{BAUD, Port, USB_PID, USB_VID};

/// DCB flags: binary mode, DTR and RTS asserted (the official app's line state 0x0003),
/// no flow control.
const DCB_BINARY: u32 = 1;
const DCB_DTR_ENABLE: u32 = 1 << 4;
const DCB_RTS_ENABLE: u32 = 1 << 12;
/// Write deadline for one chunk, as SparkyMK2 uses.
const WRITE_TIMEOUT_MS: u32 = 5000;

fn os_error(e: windows::core::Error) -> io::Error {
    let code = e.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

pub struct SerialPort {
    handle: HANDLE,
    read_event: HANDLE,
    /// The write event; the lock also keeps writes whole.
    write_event: Mutex<HANDLE>,
}

// SAFETY: the handles are kernel objects usable from any thread; writes are serialised
// by the mutex, and only the reader thread reads.
unsafe impl Send for SerialPort {}
unsafe impl Sync for SerialPort {}

impl SerialPort {
    pub fn open(name: &str) -> io::Result<Self> {
        let path = HSTRING::from(format!("\\\\.\\{name}"));
        // SAFETY: plain Win32 calls on a handle owned by the returned value.
        unsafe {
            let handle = CreateFileW(
                &path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .map_err(os_error)?;
            let port = SerialPort {
                handle,
                read_event: CreateEventW(None, true, false, None).map_err(os_error)?,
                write_event: Mutex::new(CreateEventW(None, true, false, None).map_err(os_error)?),
            };
            let mut dcb = DCB {
                DCBlength: size_of::<DCB>() as u32,
                ..Default::default()
            };
            GetCommState(handle, &mut dcb).map_err(os_error)?;
            dcb.BaudRate = BAUD;
            dcb._bitfield = DCB_BINARY | DCB_DTR_ENABLE | DCB_RTS_ENABLE;
            dcb.ByteSize = 8;
            dcb.Parity = NOPARITY;
            dcb.StopBits = ONESTOPBIT;
            SetCommState(handle, &dcb).map_err(os_error)?;
            // Reads return as soon as any byte is available, and otherwise wait for one.
            // Windows has no unbounded form of this mode; the longest wait it allows is
            // MAXDWORD - 1 ms (49.7 days), after which the reader simply reads again.
            let timeouts = COMMTIMEOUTS {
                ReadIntervalTimeout: u32::MAX,
                ReadTotalTimeoutMultiplier: u32::MAX,
                ReadTotalTimeoutConstant: u32::MAX - 1,
                WriteTotalTimeoutMultiplier: 0,
                WriteTotalTimeoutConstant: WRITE_TIMEOUT_MS,
            };
            SetCommTimeouts(handle, &timeouts).map_err(os_error)?;
            PurgeComm(handle, PURGE_RXCLEAR).map_err(os_error)?;
            Ok(port)
        }
    }

    /// Start an overlapped transfer and wait for it to finish.
    fn transfer(
        &self,
        event: HANDLE,
        start: impl FnOnce(&mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<usize> {
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        if let Err(e) = start(&mut ov)
            && e.code() != ERROR_IO_PENDING.to_hresult()
        {
            return Err(os_error(e));
        }
        let mut n = 0u32;
        // SAFETY: `ov` lives on this frame until the transfer completes; bWait blocks on
        // its event.
        unsafe { GetOverlappedResult(self.handle, &ov, &mut n, true) }.map_err(os_error)?;
        Ok(n as usize)
    }
}

impl Port for SerialPort {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: the buffer outlives the transfer, which `transfer` waits for.
        self.transfer(self.read_event, |ov| unsafe {
            ReadFile(self.handle, Some(buf), None, Some(ov))
        })
    }

    fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        let event = self.write_event.lock().unwrap();
        while !bytes.is_empty() {
            // SAFETY: as for read.
            let n = self.transfer(*event, |ov| unsafe {
                WriteFile(self.handle, Some(bytes), None, Some(ov))
            })?;
            if n == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }

    fn cancel_read(&self) {
        // SAFETY: cancelling I/O on our own handle; the waiting reader sees
        // ERROR_OPERATION_ABORTED.
        let _ = unsafe { CancelIoEx(self.handle, None) };
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        // SAFETY: closing handles this value owns; no transfer is pending once the
        // Link (which owns the reader) is gone.
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.read_event);
            let _ = CloseHandle(*self.write_event.lock().unwrap());
        }
    }
}

#[derive(Debug, Clone)]
pub struct PortInfo {
    /// `COM4`.
    pub name: String,
    /// `USB\VID_0582&PID_02E7\...`.
    pub instance_id: String,
}

/// Serial ports that belong to an SP-404MKII.
pub fn find_ports() -> io::Result<Vec<PortInfo>> {
    let wanted = format!("VID_{USB_VID:04X}&PID_{USB_PID:04X}");
    let mut found = Vec::new();
    // SAFETY: SetupAPI enumeration; the set is destroyed before returning.
    unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_COMPORT),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(os_error)?;
        let mut index = 0;
        loop {
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, index, &mut info).is_err() {
                break;
            }
            index += 1;
            let mut id = [0u16; 200];
            if SetupDiGetDeviceInstanceIdW(set, &info, Some(&mut id), None).is_err() {
                continue;
            }
            let end = id.iter().position(|&c| c == 0).unwrap_or(id.len());
            let instance_id = String::from_utf16_lossy(&id[..end]);
            if !instance_id.to_ascii_uppercase().contains(&wanted) {
                continue;
            }
            let Ok(key) =
                SetupDiOpenDevRegKey(set, &info, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0)
            else {
                continue;
            };
            let mut name = [0u16; 64];
            let mut len = size_of_val(&name) as u32;
            let read = RegQueryValueExW(
                key,
                w!("PortName"),
                None,
                None,
                Some(name.as_mut_ptr().cast()),
                Some(&mut len),
            );
            let _ = RegCloseKey(key);
            if read == ERROR_SUCCESS {
                let name = String::from_utf16_lossy(&name[..len as usize / 2]);
                found.push(PortInfo {
                    name: name.trim_end_matches('\0').to_string(),
                    instance_id,
                });
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    Ok(found)
}
