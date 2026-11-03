//! Typed access to Core Audio HAL object properties from a client process.

use std::ffi::c_void;
use std::ptr::{NonNull, null};

use objc2_core_audio::*;
use objc2_core_foundation::{CFRetained, CFString};

pub type Id = AudioObjectID;
pub type Status = i32;

pub const SYSTEM: Id = kAudioObjectSystemObject as Id;
pub const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;
pub const OUTPUT: u32 = kAudioObjectPropertyScopeOutput;
/// kAudioHardwarePropertyRunLoop, from the deprecated header.
const RUN_LOOP: u32 = u32::from_be_bytes(*b"rnlp");

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain }
}

fn check(status: Status) -> Result<(), Status> {
    if status == 0 { Ok(()) } else { Err(status) }
}

/// Read a property into `out`, qualified by `qualifier`; returns the bytes read.
fn read<Q>(object: Id, selector: u32, scope: u32, qualifier: Option<&Q>, out: &mut [u8]) -> Result<usize, Status> {
    let address = address(selector, scope);
    let (q_size, q_data) = qualifier.map_or((0, null()), |q| (size_of::<Q>() as u32, (q as *const Q).cast()));
    let mut size = out.len() as u32;
    let out = NonNull::new(out.as_mut_ptr().cast::<c_void>()).unwrap_or(NonNull::dangling());
    check(unsafe {
        AudioObjectGetPropertyData(object, NonNull::from(&address), q_size, q_data, NonNull::from(&mut size), out)
    })?;
    Ok(size as usize)
}

fn read_value<T: Copy, Q>(object: Id, selector: u32, scope: u32, qualifier: Option<&Q>) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let bytes = unsafe { std::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    let read = read(object, selector, scope, qualifier, bytes).ok()?;
    (read == size_of::<T>()).then(|| unsafe { value.assume_init() })
}

pub fn get<T: Copy>(object: Id, selector: u32, scope: u32) -> Option<T> {
    read_value::<T, ()>(object, selector, scope, None)
}

pub fn get_vec<T: Copy + Default>(object: Id, selector: u32, scope: u32) -> Vec<T> {
    let address = address(selector, scope);
    let mut size = 0u32;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, NonNull::from(&address), 0, null(), NonNull::from(&mut size)) };
    if status != 0 {
        return Vec::new();
    }
    let mut items = vec![T::default(); size as usize / size_of::<T>()];
    let bytes = unsafe { std::slice::from_raw_parts_mut(items.as_mut_ptr().cast::<u8>(), size_of_val(&items[..])) };
    match read::<()>(object, selector, scope, None, bytes) {
        Ok(read) => {
            items.truncate(read / size_of::<T>());
            items
        }
        Err(_) => Vec::new(),
    }
}

/// A CFString property, which the HAL hands out at +1.
pub fn get_string(object: Id, selector: u32) -> Option<String> {
    let string: *mut CFString = get(object, selector, GLOBAL)?;
    let string = unsafe { CFRetained::from_raw(NonNull::new(string)?) };
    Some(string.to_string())
}

/// A property that translates a string (a UID, a bundle ID) to an object.
pub fn translate(selector: u32, key: &str) -> Option<Id> {
    let key = CFString::from_str(key);
    let key_ref: *const CFString = &*key;
    read_value::<Id, _>(SYSTEM, selector, GLOBAL, Some(&key_ref)).filter(|&id| id != kAudioObjectUnknown)
}

pub fn set<T>(object: Id, selector: u32, scope: u32, value: &T) -> Result<(), Status> {
    let address = address(selector, scope);
    check(unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(&address),
            0,
            null(),
            size_of::<T>() as u32,
            NonNull::from(value).cast(),
        )
    })
}

pub fn set_string(object: Id, selector: u32, value: &str) -> Result<(), Status> {
    let value = CFString::from_str(value);
    let value_ref: *const CFString = &*value;
    set(object, selector, GLOBAL, &value_ref)
}

/// Deliver property notifications on the HAL's own thread instead of the
/// main run loop, which this process never runs.
pub fn notify_on_hal_thread() -> Result<(), Status> {
    set(SYSTEM, RUN_LOOP, GLOBAL, &null::<c_void>())
}

type Callback = Box<dyn Fn() + Send + Sync>;

unsafe extern "C-unwind" fn trampoline(_: Id, _: u32, _: NonNull<AudioObjectPropertyAddress>, data: *mut c_void) -> Status {
    unsafe { (*data.cast::<Callback>())() };
    0
}

/// Call `callback` whenever the property changes. The registration lives as
/// long as the object does.
pub fn listen(object: Id, selector: u32, scope: u32, callback: impl Fn() + Send + Sync + 'static) -> Result<(), Status> {
    let address = address(selector, scope);
    let data = Box::into_raw(Box::new(Box::new(callback) as Callback));
    check(unsafe { AudioObjectAddPropertyListener(object, NonNull::from(&address), Some(trampoline), data.cast()) })
}
