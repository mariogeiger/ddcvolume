//! Core Audio server plug-in: the CFPlugIn factory and the
//! AudioServerPlugInDriverInterface table, dispatching property calls to the
//! object model and IO calls to the loopback ring.

#![allow(non_upper_case_globals)] // Core Audio's constant names, as in its headers.

#[allow(dead_code)] // The bundle ID is the helper's half of the contract.
mod contract;
mod io;
mod model;

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering::Relaxed};

use objc2_core_audio::*;
use objc2_core_audio_types::AudioTimeStamp;
use objc2_core_foundation::CFUUIDBytes;

use model::{Change, OUTPUT, PLUGIN, TAP};

/// Core Audio status code.
pub(crate) type OSStatus = i32;
type Driver = *const c_void;
type Address = *const AudioObjectPropertyAddress;

/// kAudioServerPlugInTypeUUID
const PLUGIN_TYPE: [u8; 16] = *b"\x44\x3A\xBA\xB8\xE7\xB3\x49\x1A\xB9\x85\xBE\xB9\x18\x70\x30\xDB";
/// kAudioServerPlugInDriverInterfaceUUID
const DRIVER_INTERFACE: [u8; 16] = *b"\xEE\xA5\x77\x3D\xCC\x43\x49\xF1\x8E\x00\x8F\x96\xE7\xD2\x3B\x17";
/// IUnknownUUID
const IUNKNOWN: [u8; 16] = *b"\x00\x00\x00\x00\x00\x00\x00\x00\xC0\x00\x00\x00\x00\x00\x00\x46";
const E_NOINTERFACE: i32 = 0x8000_0004_u32 as i32;

const READ_INPUT: u32 = u32::from_be_bytes(*b"read");
const WRITE_MIX: u32 = u32::from_be_bytes(*b"rite");

/// The prefix of AudioServerPlugInHostInterface this plug-in calls.
#[repr(C)]
struct Host {
    properties_changed: unsafe extern "C" fn(*const Host, AudioObjectID, u32, Address) -> OSStatus,
}

/// AudioServerPlugInIOCycleInfo
#[repr(C)]
struct IOCycleInfo {
    counter: u64,
    nominal_frames: u32,
    current_time: AudioTimeStamp,
    input_time: AudioTimeStamp,
    output_time: AudioTimeStamp,
    main_ticks_per_frame: f64,
    device_ticks_per_frame: f64,
}

static HOST: AtomicPtr<Host> = AtomicPtr::new(null_mut());

unsafe extern "C" {
    fn CFUUIDGetUUIDBytes(uuid: *const c_void) -> CFUUIDBytes;
}

fn bytes(uuid: CFUUIDBytes) -> [u8; 16] {
    unsafe { std::mem::transmute(uuid) }
}

/// Tell the host that properties of an object changed.
fn notify(object: AudioObjectID, selectors: &[u32]) {
    let host = HOST.load(Relaxed);
    if host.is_null() {
        return;
    }
    let addresses: Vec<_> = selectors
        .iter()
        .map(|&s| AudioObjectPropertyAddress {
            mSelector: s,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        })
        .collect();
    unsafe { ((*host).properties_changed)(host, object, addresses.len() as u32, addresses.as_ptr()) };
}

fn announce(change: Change) {
    match change {
        Change::None => {}
        Change::Object(object, selectors) => notify(object, selectors),
        // Changing the device list from inside a property call would deadlock the host.
        Change::Devices => {
            std::thread::spawn(|| {
                notify(PLUGIN, &[kAudioPlugInPropertyDeviceList, kAudioObjectPropertyOwnedObjects]);
                for device in [OUTPUT, TAP] {
                    if model::exists(device) {
                        notify(device, &[kAudioObjectPropertyName]);
                    }
                }
            });
        }
    }
}

/// View raw (pointer, size) arguments as bytes; null is empty.
unsafe fn raw<'a>(data: *const c_void, size: u32) -> &'a [u8] {
    if data.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(data.cast(), size as usize) } }
}

fn lookup(object: AudioObjectID, address: Address, qualifier: &[u8], input: &[u8]) -> Result<model::Value, OSStatus> {
    if !model::exists(object) {
        return Err(kAudioHardwareBadObjectError);
    }
    model::get(object, unsafe { &*address }, qualifier, input).ok_or(kAudioHardwareUnknownPropertyError)
}

// --- IUnknown ---

unsafe extern "C" fn query_interface(_: *mut c_void, iid: CFUUIDBytes, out: *mut *mut c_void) -> i32 {
    let found = matches!(bytes(iid), IUNKNOWN | DRIVER_INTERFACE);
    unsafe { *out = if found { driver() } else { null_mut() } };
    if found { 0 } else { E_NOINTERFACE }
}

/// The driver is a static singleton, so reference counts are moot.
unsafe extern "C" fn add_ref(_: *mut c_void) -> u32 {
    1
}

unsafe extern "C" fn release(_: *mut c_void) -> u32 {
    1
}

// --- Life cycle ---

unsafe extern "C" fn initialize(_: Driver, host: *const Host) -> OSStatus {
    HOST.store(host.cast_mut(), Relaxed);
    0
}

unsafe extern "C" fn create_device(_: Driver, _: *const c_void, _: *const c_void, _: *mut AudioObjectID) -> OSStatus {
    kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn destroy_device(_: Driver, _: AudioObjectID) -> OSStatus {
    kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn device_client(_: Driver, _: AudioObjectID, _: *const c_void) -> OSStatus {
    0
}

unsafe extern "C" fn configuration_change(_: Driver, _: AudioObjectID, _: u64, _: *mut c_void) -> OSStatus {
    0
}

// --- Properties ---

unsafe extern "C" fn has_property(_: Driver, object: AudioObjectID, _: i32, address: Address) -> u8 {
    lookup(object, address, &[], &[]).is_ok() as u8
}

unsafe extern "C" fn is_property_settable(_: Driver, object: AudioObjectID, _: i32, address: Address, out: *mut u8) -> OSStatus {
    match lookup(object, address, &[], &[]) {
        Ok(_) => {
            unsafe { *out = model::settable(object, (*address).mSelector) as u8 };
            0
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn get_property_data_size(
    _: Driver,
    object: AudioObjectID,
    _: i32,
    address: Address,
    qualifier_size: u32,
    qualifier: *const c_void,
    out_size: *mut u32,
) -> OSStatus {
    match lookup(object, address, unsafe { raw(qualifier, qualifier_size) }, &[]) {
        Ok(value) => {
            unsafe { *out_size = value.size() };
            0
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn get_property_data(
    _: Driver,
    object: AudioObjectID,
    _: i32,
    address: Address,
    qualifier_size: u32,
    qualifier: *const c_void,
    size: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    let value = lookup(object, address, unsafe { raw(qualifier, qualifier_size) }, unsafe { raw(out, size) });
    match value.and_then(|v| unsafe { v.write(out, size) }) {
        Ok(written) => {
            unsafe { *out_size = written };
            0
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn set_property_data(
    _: Driver,
    object: AudioObjectID,
    _: i32,
    address: Address,
    _: u32,
    _: *const c_void,
    size: u32,
    data: *const c_void,
) -> OSStatus {
    if let Err(status) = lookup(object, address, &[], &[]) {
        return status;
    }
    let selector = unsafe { (*address).mSelector };
    if !model::settable(object, selector) {
        return kAudioHardwareIllegalOperationError;
    }
    match model::set(object, selector, unsafe { raw(data, size) }) {
        Ok(change) => {
            announce(change);
            0
        }
        Err(status) => status,
    }
}

// --- IO ---

unsafe extern "C" fn start_io(_: Driver, device: AudioObjectID, _: u32) -> OSStatus {
    if !model::io_running(device, 1) {
        return kAudioHardwareBadObjectError;
    }
    io::start();
    0
}

unsafe extern "C" fn stop_io(_: Driver, device: AudioObjectID, _: u32) -> OSStatus {
    if !model::io_running(device, -1) {
        return kAudioHardwareBadObjectError;
    }
    io::stop();
    0
}

unsafe extern "C" fn get_zero_time_stamp(
    _: Driver,
    _: AudioObjectID,
    _: u32,
    sample_time: *mut f64,
    host_time: *mut u64,
    seed: *mut u64,
) -> OSStatus {
    let (sample, host) = io::zero_time_stamp();
    unsafe {
        *sample_time = sample;
        *host_time = host;
        *seed = 1;
    }
    0
}

unsafe extern "C" fn will_do_io_operation(
    _: Driver,
    device: AudioObjectID,
    _: u32,
    operation: u32,
    will_do: *mut u8,
    in_place: *mut u8,
) -> OSStatus {
    unsafe {
        *will_do = matches!((device, operation), (OUTPUT, WRITE_MIX) | (TAP, READ_INPUT)) as u8;
        *in_place = 1;
    }
    0
}

unsafe extern "C" fn io_operation_bracket(_: Driver, _: AudioObjectID, _: u32, _: u32, _: u32, _: *const IOCycleInfo) -> OSStatus {
    0
}

unsafe extern "C" fn do_io_operation(
    _: Driver,
    device: AudioObjectID,
    _: AudioObjectID,
    _: u32,
    operation: u32,
    frames: u32,
    cycle: *const IOCycleInfo,
    main: *mut c_void,
    _: *mut c_void,
) -> OSStatus {
    let samples = unsafe { std::slice::from_raw_parts_mut(main.cast::<f32>(), frames as usize * io::CHANNELS) };
    let cycle = unsafe { &*cycle };
    match (device, operation) {
        (OUTPUT, WRITE_MIX) => io::write(cycle.output_time.mSampleTime, samples, model::muted()),
        (TAP, READ_INPUT) => io::read(cycle.input_time.mSampleTime, samples),
        _ => {}
    }
    0
}

// --- The interface table and the factory ---

#[repr(C)]
struct Interface {
    reserved: usize,
    query_interface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "C" fn(*mut c_void) -> u32,
    release: unsafe extern "C" fn(*mut c_void) -> u32,
    initialize: unsafe extern "C" fn(Driver, *const Host) -> OSStatus,
    create_device: unsafe extern "C" fn(Driver, *const c_void, *const c_void, *mut AudioObjectID) -> OSStatus,
    destroy_device: unsafe extern "C" fn(Driver, AudioObjectID) -> OSStatus,
    add_device_client: unsafe extern "C" fn(Driver, AudioObjectID, *const c_void) -> OSStatus,
    remove_device_client: unsafe extern "C" fn(Driver, AudioObjectID, *const c_void) -> OSStatus,
    perform_configuration_change: unsafe extern "C" fn(Driver, AudioObjectID, u64, *mut c_void) -> OSStatus,
    abort_configuration_change: unsafe extern "C" fn(Driver, AudioObjectID, u64, *mut c_void) -> OSStatus,
    has_property: unsafe extern "C" fn(Driver, AudioObjectID, i32, Address) -> u8,
    is_property_settable: unsafe extern "C" fn(Driver, AudioObjectID, i32, Address, *mut u8) -> OSStatus,
    get_property_data_size:
        unsafe extern "C" fn(Driver, AudioObjectID, i32, Address, u32, *const c_void, *mut u32) -> OSStatus,
    get_property_data: unsafe extern "C" fn(
        Driver,
        AudioObjectID,
        i32,
        Address,
        u32,
        *const c_void,
        u32,
        *mut u32,
        *mut c_void,
    ) -> OSStatus,
    set_property_data:
        unsafe extern "C" fn(Driver, AudioObjectID, i32, Address, u32, *const c_void, u32, *const c_void) -> OSStatus,
    start_io: unsafe extern "C" fn(Driver, AudioObjectID, u32) -> OSStatus,
    stop_io: unsafe extern "C" fn(Driver, AudioObjectID, u32) -> OSStatus,
    get_zero_time_stamp: unsafe extern "C" fn(Driver, AudioObjectID, u32, *mut f64, *mut u64, *mut u64) -> OSStatus,
    will_do_io_operation: unsafe extern "C" fn(Driver, AudioObjectID, u32, u32, *mut u8, *mut u8) -> OSStatus,
    begin_io_operation: unsafe extern "C" fn(Driver, AudioObjectID, u32, u32, u32, *const IOCycleInfo) -> OSStatus,
    do_io_operation: unsafe extern "C" fn(
        Driver,
        AudioObjectID,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const IOCycleInfo,
        *mut c_void,
        *mut c_void,
    ) -> OSStatus,
    end_io_operation: unsafe extern "C" fn(Driver, AudioObjectID, u32, u32, u32, *const IOCycleInfo) -> OSStatus,
}

static INTERFACE: Interface = Interface {
    reserved: 0,
    query_interface,
    add_ref,
    release,
    initialize,
    create_device,
    destroy_device,
    add_device_client: device_client,
    remove_device_client: device_client,
    perform_configuration_change: configuration_change,
    abort_configuration_change: configuration_change,
    has_property,
    is_property_settable,
    get_property_data_size,
    get_property_data,
    set_property_data,
    start_io,
    stop_io,
    get_zero_time_stamp,
    will_do_io_operation,
    begin_io_operation: io_operation_bracket,
    do_io_operation,
    end_io_operation: io_operation_bracket,
};

/// AudioServerPlugInDriverRef: a pointer to a pointer to the table.
#[allow(dead_code)] // Read by the host through the pointer.
struct DriverRef(&'static Interface);
static DRIVER: DriverRef = DriverRef(&INTERFACE);

fn driver() -> *mut c_void {
    (&raw const DRIVER).cast_mut().cast()
}

/// CFPlugIn factory named in Info.plist.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ddcvolume_create(_: *const c_void, requested_type: *const c_void) -> *mut c_void {
    if bytes(unsafe { CFUUIDGetUUIDBytes(requested_type) }) == PLUGIN_TYPE { driver() } else { null_mut() }
}
