//! DDC/CI over the Apple Silicon display coprocessor: read and write VESA MCCS
//! VCP features of external displays through the private IOAVService API.

use std::ffi::{c_char, c_void, CStr};
use std::thread::sleep;
use std::time::Duration;

type CFTypeRef = *const c_void;
type IoObject = u32;

#[link(name = "IOKit", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn IORegistryGetRootEntry(main_port: u32) -> IoObject;
    fn IORegistryEntryCreateIterator(entry: IoObject, plane: *const c_char, options: u32, iterator: *mut IoObject) -> i32;
    fn IOIteratorNext(iterator: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> i32;
    fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> i32;
    fn IORegistryEntryCreateCFProperty(entry: IoObject, key: CFTypeRef, allocator: CFTypeRef, options: u32) -> CFTypeRef;
    fn IOAVServiceCreateWithService(allocator: CFTypeRef, service: IoObject) -> CFTypeRef;
    fn IOAVServiceReadI2C(service: CFTypeRef, chip: u32, offset: u32, buffer: *mut u8, size: u32) -> i32;
    fn IOAVServiceWriteI2C(service: CFTypeRef, chip: u32, offset: u32, buffer: *const u8, size: u32) -> i32;
    fn CFStringCreateWithCString(allocator: CFTypeRef, string: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
    fn CFRelease(object: CFTypeRef);
}

const MAIN_PORT_DEFAULT: u32 = 0;
const ITERATE_RECURSIVELY: u32 = 1;
const UTF8: u32 = 0x0800_0100;

/// 7-bit I2C address of the DDC/CI endpoint in the display.
const DISPLAY_CHIP: u32 = 0x37;
/// Host source address, sent as the I2C sub-address of every packet.
const HOST: u8 = 0x51;
/// 8-bit write address of the display, folded into the checksum.
const DISPLAY_WRITE: u8 = 0x6E;
/// Host address the display replies to, folded into the reply checksum.
const HOST_REPLY: u8 = 0x50;

/// MCCS VCP code of the audio speaker volume.
pub const VOLUME: u8 = 0x62;

const OP_GET_VCP: u8 = 0x01;
const OP_GET_VCP_REPLY: u8 = 0x02;
const OP_SET_VCP: u8 = 0x03;

/// Minimum delay the MCCS spec requires between a request and its reply.
const REPLY_DELAY: Duration = Duration::from_millis(50);
const ATTEMPTS: usize = 4;

/// A VCP feature reading: the current value and the display's maximum.
#[derive(Clone, Copy, Debug)]
pub struct Vcp {
    pub current: u16,
    pub max: u16,
}

/// An external display reachable over DDC/CI.
pub struct Display(CFTypeRef);

impl Drop for Display {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn cfstring(s: &CStr) -> CFTypeRef {
    unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) }
}

/// True when the registry entry is a display AV service on an external port.
fn is_external_av_service(entry: IoObject) -> bool {
    let mut name = [0 as c_char; 128];
    unsafe { IORegistryEntryGetName(entry, name.as_mut_ptr()) };
    if unsafe { CStr::from_ptr(name.as_ptr()) } != c"DCPAVServiceProxy" {
        return false;
    }
    let key = cfstring(c"Location");
    let external = cfstring(c"External");
    let location = unsafe { IORegistryEntryCreateCFProperty(entry, key, std::ptr::null(), 0) };
    let matches = !location.is_null() && unsafe { CFEqual(location, external) } != 0;
    unsafe {
        if !location.is_null() {
            CFRelease(location);
        }
        CFRelease(key);
        CFRelease(external);
    }
    matches
}

/// All external displays, in registry order.
pub fn displays() -> Vec<Display> {
    let mut found = Vec::new();
    let mut iterator: IoObject = 0;
    unsafe {
        let root = IORegistryGetRootEntry(MAIN_PORT_DEFAULT);
        IORegistryEntryCreateIterator(root, c"IOService".as_ptr(), ITERATE_RECURSIVELY, &mut iterator);
        loop {
            let entry = IOIteratorNext(iterator);
            if entry == 0 {
                break;
            }
            if is_external_av_service(entry) {
                let service = IOAVServiceCreateWithService(std::ptr::null(), entry);
                if !service.is_null() {
                    found.push(Display(service));
                }
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iterator);
    }
    found
}

/// XOR checksum over the packet, seeded with the addresses it travels between.
fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |acc, b| acc ^ b)
}

impl Display {
    /// Send one DDC/CI message: length byte, payload, checksum.
    fn send(&self, payload: &[u8]) -> Result<(), String> {
        let mut packet = Vec::with_capacity(payload.len() + 2);
        packet.push(0x80 | payload.len() as u8);
        packet.extend_from_slice(payload);
        packet.push(checksum(DISPLAY_WRITE ^ HOST, &packet));
        let status = unsafe {
            IOAVServiceWriteI2C(self.0, DISPLAY_CHIP, HOST as u32, packet.as_ptr(), packet.len() as u32)
        };
        if status == 0 { Ok(()) } else { Err(format!("I2C write failed ({status:#x})")) }
    }

    /// One get-VCP round trip; the reply is
    /// `6E 88 02 result code type maxH maxL curH curL checksum`.
    fn query(&self, code: u8) -> Result<Vcp, String> {
        self.send(&[OP_GET_VCP, code])?;
        sleep(REPLY_DELAY);
        let mut reply = [0u8; 11];
        let status = unsafe {
            IOAVServiceReadI2C(self.0, DISPLAY_CHIP, HOST as u32, reply.as_mut_ptr(), reply.len() as u32)
        };
        if status != 0 {
            return Err(format!("I2C read failed ({status:#x})"));
        }
        let [source, length, op, result, echoed, _, max_h, max_l, cur_h, cur_l, sum] = reply;
        if source != DISPLAY_WRITE || length != 0x88 || op != OP_GET_VCP_REPLY || echoed != code {
            return Err(format!("malformed reply {reply:02x?}"));
        }
        if checksum(HOST_REPLY, &reply[..10]) != sum {
            return Err(format!("bad reply checksum {reply:02x?}"));
        }
        if result != 0 {
            return Err(format!("display does not support VCP {code:#04x}"));
        }
        Ok(Vcp { current: u16::from_be_bytes([cur_h, cur_l]), max: u16::from_be_bytes([max_h, max_l]) })
    }

    /// Read a VCP feature, retrying the noisy bus a few times.
    pub fn get(&self, code: u8) -> Result<Vcp, String> {
        let mut error = String::new();
        for _ in 0..ATTEMPTS {
            match self.query(code) {
                Ok(vcp) => return Ok(vcp),
                Err(e) => error = e,
            }
            sleep(REPLY_DELAY);
        }
        Err(error)
    }

    /// Write a VCP feature.
    pub fn set(&self, code: u8, value: u16) -> Result<(), String> {
        let [high, low] = value.to_be_bytes();
        self.send(&[OP_SET_VCP, code, high, low])?;
        sleep(REPLY_DELAY);
        Ok(())
    }
}
