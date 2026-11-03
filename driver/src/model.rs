//! The plug-in's audio object tree and its properties.
//!
//!     plug-in 1
//!     ├── output device 2 ── output stream 3, volume 4, mute 5
//!     └── tap device 6 (hidden) ── input stream 7
//!
//! Every property is answered by one function, `get`, so presence, size and
//! data can never disagree.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

use objc2_core_audio::*;
use objc2_core_audio_types::*;
use objc2_core_foundation::{CFRetained, CFString};

use crate::OSStatus;
use crate::contract::{OUTPUT_UID, TAP_UID, TARGET_NAME};
use crate::io::{CHANNELS, RING_FRAMES, SAMPLE_RATE};

pub const PLUGIN: AudioObjectID = 1;
pub const OUTPUT: AudioObjectID = 2;
const OUTPUT_STREAM: AudioObjectID = 3;
const VOLUME: AudioObjectID = 4;
const MUTE: AudioObjectID = 5;
pub const TAP: AudioObjectID = 6;
const TAP_STREAM: AudioObjectID = 7;

const MANUFACTURER: &str = "ddcvolume";
const MODEL_UID: &str = "ddcvolume:model";
/// Decibels shown at volume 0; the screen applies its own curve over DDC.
const MIN_DB: f32 = -64.0;

static TARGET: Mutex<String> = Mutex::new(String::new());
static VOLUME_SCALAR: AtomicU32 = AtomicU32::new(0x3f00_0000); // 0.5
static MUTED: AtomicBool = AtomicBool::new(false);
static RUNNING: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

struct Device {
    id: AudioObjectID,
    stream: AudioObjectID,
    /// The tap is the hidden input half of the loopback.
    tap: bool,
    uid: &'static str,
    controls: &'static [AudioObjectID],
}

const DEVICES: [Device; 2] = [
    Device { id: OUTPUT, stream: OUTPUT_STREAM, tap: false, uid: OUTPUT_UID, controls: &[VOLUME, MUTE] },
    Device { id: TAP, stream: TAP_STREAM, tap: true, uid: TAP_UID, controls: &[] },
];

/// The device that owns a device or stream object.
fn device(id: AudioObjectID) -> Option<&'static Device> {
    DEVICES.iter().find(|d| d.id == id || d.stream == id)
}

fn target() -> String {
    TARGET.lock().unwrap().clone()
}

fn published() -> bool {
    !TARGET.lock().unwrap().is_empty()
}

pub fn exists(id: AudioObjectID) -> bool {
    id == PLUGIN || (published() && (OUTPUT..=TAP_STREAM).contains(&id))
}

pub fn muted() -> bool {
    MUTED.load(Relaxed)
}

/// Track IO clients per device; returns false for an unknown device.
pub fn io_running(id: AudioObjectID, delta: isize) -> bool {
    match DEVICES.iter().position(|d| d.id == id) {
        Some(i) if exists(id) => {
            RUNNING[i].fetch_add(delta as usize, Relaxed);
            true
        }
        _ => false,
    }
}

/// A property value in the form the host copies out.
pub enum Value {
    /// Plain data: an array of `element`-byte items, or one item.
    Pod { bytes: Vec<u8>, element: usize },
    /// Handed to the host as a +1 CFStringRef.
    String(String),
}

fn array<T: Copy>(items: &[T]) -> Option<Value> {
    let bytes = unsafe { std::slice::from_raw_parts(items.as_ptr().cast::<u8>(), size_of_val(items)) };
    Some(Value::Pod { bytes: bytes.to_vec(), element: size_of::<T>() })
}

fn scalar<T: Copy>(item: T) -> Option<Value> {
    array(&[item])
}

fn string(s: impl Into<String>) -> Option<Value> {
    Some(Value::String(s.into()))
}

impl Value {
    pub fn size(&self) -> u32 {
        match self {
            Value::Pod { bytes, .. } => bytes.len() as u32,
            Value::String(_) => size_of::<*const CFString>() as u32,
        }
    }

    /// Copy as many whole items as fit in `capacity` bytes; returns the size written.
    ///
    /// # Safety
    /// `out` must be valid for `capacity` bytes.
    pub unsafe fn write(&self, out: *mut c_void, capacity: u32) -> Result<u32, OSStatus> {
        let capacity = capacity as usize;
        match self {
            Value::Pod { bytes, element } => {
                let fit = (capacity / element * element).min(bytes.len());
                if fit == 0 && !bytes.is_empty() {
                    return Err(kAudioHardwareBadPropertySizeError);
                }
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), fit) };
                Ok(fit as u32)
            }
            Value::String(s) => {
                if capacity < size_of::<*const CFString>() {
                    return Err(kAudioHardwareBadPropertySizeError);
                }
                let string = CFRetained::into_raw(CFString::from_str(s));
                unsafe { out.cast::<*const CFString>().write_unaligned(string.as_ptr()) };
                Ok(size_of::<*const CFString>() as u32)
            }
        }
    }
}

/// Read a CFStringRef passed by reference in property or qualifier data.
fn cfstring_arg(bytes: &[u8]) -> Option<String> {
    let ptr = bytes.get(..size_of::<usize>())?.as_ptr().cast::<*const CFString>();
    let string = unsafe { ptr.read_unaligned() };
    (!string.is_null()).then(|| unsafe { &*string }.to_string())
}

fn f32_arg(bytes: &[u8]) -> f32 {
    bytes.get(..4).map_or(0.0, |b| f32::from_ne_bytes(b.try_into().unwrap()))
}

fn to_db(scalar: f32) -> f32 {
    MIN_DB * (1.0 - scalar.clamp(0.0, 1.0))
}

fn to_scalar(db: f32) -> f32 {
    1.0 - db.clamp(MIN_DB, 0.0) / MIN_DB
}

const FORMAT: AudioStreamBasicDescription = AudioStreamBasicDescription {
    mSampleRate: SAMPLE_RATE,
    mFormatID: kAudioFormatLinearPCM,
    mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
    mBytesPerPacket: (CHANNELS * 4) as u32,
    mFramesPerPacket: 1,
    mBytesPerFrame: (CHANNELS * 4) as u32,
    mChannelsPerFrame: CHANNELS as u32,
    mBitsPerChannel: 32,
    mReserved: 0,
};

const RATE: AudioValueRange = AudioValueRange { mMinimum: SAMPLE_RATE, mMaximum: SAMPLE_RATE };

/// The value of a property, or None when the object does not have it.
/// `qualifier` is the qualifier data; `input` is the incoming contents of the
/// data buffer, which the conversion properties read their argument from.
pub fn get(id: AudioObjectID, address: &AudioObjectPropertyAddress, qualifier: &[u8], input: &[u8]) -> Option<Value> {
    if !exists(id) {
        return None;
    }
    let selector = address.mSelector;
    match id {
        PLUGIN => plugin(selector, qualifier),
        OUTPUT | TAP => device_property(device(id)?, selector, address.mScope),
        OUTPUT_STREAM | TAP_STREAM => stream_property(device(id)?, selector),
        VOLUME => volume_property(selector, input),
        MUTE => mute_property(selector),
        _ => None,
    }
}

fn published_devices() -> Vec<AudioObjectID> {
    if published() { DEVICES.iter().map(|d| d.id).collect() } else { Vec::new() }
}

fn plugin(selector: u32, qualifier: &[u8]) -> Option<Value> {
    match selector {
        kAudioObjectPropertyBaseClass => scalar(kAudioObjectClassID),
        kAudioObjectPropertyClass => scalar(kAudioPlugInClassID),
        kAudioObjectPropertyOwner => scalar(kAudioObjectUnknown),
        kAudioObjectPropertyManufacturer => string(MANUFACTURER),
        kAudioObjectPropertyOwnedObjects | kAudioPlugInPropertyDeviceList => array(&published_devices()),
        kAudioPlugInPropertyTranslateUIDToDevice => {
            let uid = cfstring_arg(qualifier);
            let found = DEVICES.iter().find(|d| published() && uid.as_deref() == Some(d.uid));
            scalar(found.map_or(kAudioObjectUnknown, |d| d.id))
        }
        kAudioPlugInPropertyResourceBundle => string(""),
        kAudioObjectPropertyCustomPropertyInfoList => scalar(AudioServerPlugInCustomPropertyInfo {
            mSelector: TARGET_NAME,
            mPropertyDataType: kAudioServerPlugInCustomPropertyDataTypeCFString,
            mQualifierDataType: kAudioServerPlugInCustomPropertyDataTypeNone,
        }),
        TARGET_NAME => string(target()),
        _ => None,
    }
}

fn device_property(d: &Device, selector: u32, scope: u32) -> Option<Value> {
    let own_scope = if d.tap { kAudioObjectPropertyScopeInput } else { kAudioObjectPropertyScopeOutput };
    let streams: &[AudioObjectID] =
        if scope == kAudioObjectPropertyScopeGlobal || scope == own_scope { std::slice::from_ref(&d.stream) } else { &[] };
    match selector {
        kAudioObjectPropertyBaseClass => scalar(kAudioObjectClassID),
        kAudioObjectPropertyClass => scalar(kAudioDeviceClassID),
        kAudioObjectPropertyOwner => scalar(PLUGIN),
        kAudioObjectPropertyName => string(if d.tap { format!("{} (tap)", target()) } else { target() }),
        kAudioObjectPropertyManufacturer => string(MANUFACTURER),
        kAudioObjectPropertyOwnedObjects => array(&[&[d.stream], d.controls].concat()),
        kAudioDevicePropertyDeviceUID => string(d.uid),
        kAudioDevicePropertyModelUID => string(MODEL_UID),
        kAudioDevicePropertyTransportType => scalar(kAudioDeviceTransportTypeVirtual),
        kAudioDevicePropertyRelatedDevices => scalar(d.id),
        kAudioDevicePropertyClockDomain => scalar(0u32),
        kAudioDevicePropertyDeviceIsAlive => scalar(1u32),
        kAudioDevicePropertyDeviceIsRunning => scalar((RUNNING[d.tap as usize].load(Relaxed) > 0) as u32),
        kAudioDevicePropertyDeviceCanBeDefaultDevice | kAudioDevicePropertyDeviceCanBeDefaultSystemDevice => {
            scalar(!d.tap as u32)
        }
        kAudioDevicePropertyLatency | kAudioDevicePropertySafetyOffset => scalar(0u32),
        kAudioDevicePropertyStreams => array(streams),
        kAudioObjectPropertyControlList => array(d.controls),
        kAudioDevicePropertyNominalSampleRate => scalar(SAMPLE_RATE),
        kAudioDevicePropertyAvailableNominalSampleRates => scalar(RATE),
        kAudioDevicePropertyIsHidden => scalar(d.tap as u32),
        kAudioDevicePropertyPreferredChannelsForStereo => array(&[1u32, 2]),
        kAudioDevicePropertyZeroTimeStampPeriod => scalar(RING_FRAMES as u32),
        _ => None,
    }
}

fn stream_property(d: &Device, selector: u32) -> Option<Value> {
    match selector {
        kAudioObjectPropertyBaseClass => scalar(kAudioObjectClassID),
        kAudioObjectPropertyClass => scalar(kAudioStreamClassID),
        kAudioObjectPropertyOwner => scalar(d.id),
        kAudioObjectPropertyOwnedObjects => array::<AudioObjectID>(&[]),
        kAudioStreamPropertyIsActive => scalar(1u32),
        kAudioStreamPropertyDirection => scalar(d.tap as u32),
        kAudioStreamPropertyTerminalType => {
            scalar(if d.tap { kAudioStreamTerminalTypeLine } else { kAudioStreamTerminalTypeSpeaker })
        }
        kAudioStreamPropertyStartingChannel => scalar(1u32),
        kAudioStreamPropertyLatency => scalar(0u32),
        kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat => scalar(FORMAT),
        kAudioStreamPropertyAvailableVirtualFormats | kAudioStreamPropertyAvailablePhysicalFormats => {
            scalar(AudioStreamRangedDescription { mFormat: FORMAT, mSampleRateRange: RATE })
        }
        _ => None,
    }
}

/// Properties every control has.
fn control_property(selector: u32, base_class: u32, class: u32) -> Option<Value> {
    match selector {
        kAudioObjectPropertyBaseClass => scalar(base_class),
        kAudioObjectPropertyClass => scalar(class),
        kAudioObjectPropertyOwner => scalar(OUTPUT),
        kAudioObjectPropertyOwnedObjects => array::<AudioObjectID>(&[]),
        kAudioControlPropertyScope => scalar(kAudioObjectPropertyScopeOutput),
        kAudioControlPropertyElement => scalar(kAudioObjectPropertyElementMain),
        _ => None,
    }
}

fn volume() -> f32 {
    f32::from_bits(VOLUME_SCALAR.load(Relaxed))
}

fn volume_property(selector: u32, input: &[u8]) -> Option<Value> {
    match selector {
        kAudioLevelControlPropertyScalarValue => scalar(volume()),
        kAudioLevelControlPropertyDecibelValue => scalar(to_db(volume())),
        kAudioLevelControlPropertyDecibelRange => scalar(AudioValueRange { mMinimum: MIN_DB as f64, mMaximum: 0.0 }),
        kAudioLevelControlPropertyConvertScalarToDecibels => scalar(to_db(f32_arg(input))),
        kAudioLevelControlPropertyConvertDecibelsToScalar => scalar(to_scalar(f32_arg(input))),
        _ => control_property(selector, kAudioLevelControlClassID, kAudioVolumeControlClassID),
    }
}

fn mute_property(selector: u32) -> Option<Value> {
    match selector {
        kAudioBooleanControlPropertyValue => scalar(muted() as u32),
        _ => control_property(selector, kAudioBooleanControlClassID, kAudioMuteControlClassID),
    }
}

pub fn settable(id: AudioObjectID, selector: u32) -> bool {
    matches!(
        (id, selector),
        (PLUGIN, TARGET_NAME)
            | (VOLUME, kAudioLevelControlPropertyScalarValue | kAudioLevelControlPropertyDecibelValue)
            | (MUTE, kAudioBooleanControlPropertyValue)
    )
}

/// What a successful set changed, for the host to be told.
pub enum Change {
    None,
    /// The device list and the device names.
    Devices,
    /// Properties of one object.
    Object(AudioObjectID, &'static [u32]),
}

/// Apply a settable property; the caller has checked `settable`.
pub fn set(id: AudioObjectID, selector: u32, data: &[u8]) -> Result<Change, OSStatus> {
    const LEVEL: &[u32] = &[kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue];
    match (id, selector) {
        (PLUGIN, TARGET_NAME) => {
            let name = cfstring_arg(data).ok_or(kAudioHardwareIllegalOperationError)?;
            let mut target = TARGET.lock().unwrap();
            if *target == name {
                return Ok(Change::None);
            }
            *target = name;
            Ok(Change::Devices)
        }
        (VOLUME, kAudioLevelControlPropertyScalarValue) => {
            VOLUME_SCALAR.store(f32_arg(data).clamp(0.0, 1.0).to_bits(), Relaxed);
            Ok(Change::Object(VOLUME, LEVEL))
        }
        (VOLUME, kAudioLevelControlPropertyDecibelValue) => {
            VOLUME_SCALAR.store(to_scalar(f32_arg(data)).to_bits(), Relaxed);
            Ok(Change::Object(VOLUME, LEVEL))
        }
        (MUTE, kAudioBooleanControlPropertyValue) => {
            MUTED.store(data.get(..4).is_some_and(|b| b != [0; 4]), Relaxed);
            Ok(Change::Object(MUTE, &[kAudioBooleanControlPropertyValue]))
        }
        _ => Err(kAudioHardwareUnknownPropertyError),
    }
}
