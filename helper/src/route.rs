//! Play the tap device's input on the display: a private aggregate device
//! clocked by the display, with drift compensation on the tap, whose IO
//! callback copies its input channels to its output channels.

use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};

use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use crate::hal::{Id, Status};

pub struct Route {
    aggregate: Id,
    proc_id: AudioDeviceIOProcID,
}

fn dictionary(entries: &[(&std::ffi::CStr, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<_> = entries.iter().map(|(k, _)| CFString::from_str(k.to_str().unwrap())).collect();
    let keys: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<&CFType> = entries.iter().map(|&(_, v)| v).collect();
    CFDictionary::from_slices(&keys, &values)
}

impl Route {
    pub fn new(display_uid: &str, tap_uid: &str) -> Result<Route, Status> {
        let one = CFNumber::new_i32(1);
        let display_uid = CFString::from_str(display_uid);
        let display = dictionary(&[(kAudioSubDeviceUIDKey, &display_uid)]);
        let tap_uid = CFString::from_str(tap_uid);
        let tap = dictionary(&[(kAudioSubDeviceUIDKey, &tap_uid), (kAudioSubDeviceDriftCompensationKey, &one)]);
        let subdevices = CFArray::<CFType>::from_objects(&[&**display, &**tap]);
        let description = dictionary(&[
            (kAudioAggregateDeviceUIDKey, &CFString::from_str("ddcvolume:route")),
            (kAudioAggregateDeviceNameKey, &CFString::from_str("ddcvolume route")),
            (kAudioAggregateDeviceIsPrivateKey, &one),
            (kAudioAggregateDeviceMainSubDeviceKey, &display_uid),
            (kAudioAggregateDeviceSubDeviceListKey, &subdevices),
        ]);

        let mut aggregate = 0;
        check(unsafe { AudioHardwareCreateAggregateDevice(description.as_opaque(), NonNull::from(&mut aggregate)) })?;
        let mut route = Route { aggregate, proc_id: None };
        check(unsafe { AudioDeviceCreateIOProcID(aggregate, Some(copy), null_mut(), NonNull::from(&mut route.proc_id)) })?;
        check(unsafe { AudioDeviceStart(aggregate, route.proc_id) })?;
        Ok(route)
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.aggregate, self.proc_id);
                AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
            }
            AudioHardwareDestroyAggregateDevice(self.aggregate);
        }
    }
}

fn check(status: Status) -> Result<(), Status> {
    if status == 0 { Ok(()) } else { Err(status) }
}

fn buffers<'a>(list: NonNull<AudioBufferList>) -> &'a [AudioBuffer] {
    let list = unsafe { list.as_ref() };
    unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) }
}

/// Sample `frame` of the list's `channel`-th channel, counting channels across
/// its interleaved float buffers; silence past the last channel.
fn sample(list: &[AudioBuffer], mut channel: usize, frame: usize) -> f32 {
    for buffer in list {
        let width = buffer.mNumberChannels as usize;
        if channel < width {
            let samples = buffer.mDataByteSize as usize / 4;
            let index = frame * width + channel;
            return if buffer.mData.is_null() || index >= samples { 0.0 } else { unsafe { *buffer.mData.cast::<f32>().add(index) } };
        }
        channel -= width;
    }
    0.0
}

unsafe extern "C-unwind" fn copy(
    _: Id,
    _: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    _: *mut c_void,
) -> Status {
    let input = buffers(input);
    let mut first = 0;
    for buffer in buffers(output) {
        let width = buffer.mNumberChannels as usize;
        if buffer.mData.is_null() || width == 0 {
            continue;
        }
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), buffer.mDataByteSize as usize / 4) };
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = sample(input, first + i % width, i / width);
        }
        first += width;
    }
    0
}
