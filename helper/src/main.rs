//! Keep the ddcvolume driver standing in for the connected display:
//! publish its devices under the display's name, route their audio to the
//! display, mirror their volume to the display over DDC/CI, and move the
//! default output from the display to its stand-in.

#![allow(non_upper_case_globals)] // Core Audio's constant names, as in its headers.

#[path = "../../driver/src/contract.rs"]
mod contract;
mod hal;
mod route;

use std::sync::mpsc::{Sender, channel};

use objc2_core_audio::*;

use contract::{OUTPUT_UID, PLUGIN_BUNDLE_ID, TAP_UID, TARGET_NAME};
use hal::{GLOBAL, Id, OUTPUT, SYSTEM};
use route::Route;

#[derive(Clone, Copy, PartialEq)]
enum Event {
    /// Devices appeared, disappeared, or the default output changed.
    Topology,
    /// The stand-in's volume changed.
    Volume,
    /// coreaudiod restarted and forgot every object.
    Restarted,
}

/// The screen being stood in for, while its audio is routed.
struct Active {
    display: Id,
    output: Id,
    _route: Route,
    ddc: Option<ddc::Display>,
    max: u16,
    sent: u16,
}

#[derive(Default)]
struct Helper {
    active: Option<Active>,
    listening: Option<Id>,
}

/// The first output device carried by a display link.
fn find_display() -> Option<Id> {
    hal::get_vec::<Id>(SYSTEM, kAudioHardwarePropertyDevices, GLOBAL).into_iter().find(|&device| {
        let transport = hal::get::<u32>(device, kAudioDevicePropertyTransportType, GLOBAL);
        matches!(transport, Some(kAudioDeviceTransportTypeDisplayPort | kAudioDeviceTransportTypeHDMI))
            && !hal::get_vec::<Id>(device, kAudioDevicePropertyStreams, OUTPUT).is_empty()
    })
}

impl Helper {
    fn reconcile(&mut self, events: &Sender<Event>) {
        let Some(plugin) = hal::translate(kAudioHardwarePropertyTranslateBundleIDToPlugIn, PLUGIN_BUNDLE_ID) else {
            eprintln!("ddcvolumed: driver {PLUGIN_BUNDLE_ID} is not loaded");
            self.active = None;
            return;
        };
        let display = find_display();
        let name = display.and_then(|d| hal::get_string(d, kAudioObjectPropertyName)).unwrap_or_default();
        if hal::get_string(plugin, TARGET_NAME).as_deref() != Some(&name) {
            // The devices appear asynchronously, and announce themselves as a topology change.
            if let Err(status) = hal::set_string(plugin, TARGET_NAME, &name) {
                eprintln!("ddcvolumed: cannot name the stand-in ({status})");
            }
        }
        let output = hal::translate(kAudioHardwarePropertyTranslateUIDToDevice, OUTPUT_UID);
        let tap = hal::translate(kAudioHardwarePropertyTranslateUIDToDevice, TAP_UID);
        let (Some(display), Some(output), Some(_)) = (display, output, tap) else {
            self.active = None;
            return;
        };
        if self.active.as_ref().is_none_or(|a| a.display != display || a.output != output) {
            self.active = None;
            self.active = self.activate(display, output);
            eprintln!("ddcvolumed: standing in for {name}");
        }
        if self.listening != Some(output) {
            let events = events.clone();
            match hal::listen(output, kAudioDevicePropertyVolumeScalar, OUTPUT, move || {
                let _ = events.send(Event::Volume);
            }) {
                Ok(()) => self.listening = Some(output),
                Err(status) => eprintln!("ddcvolumed: cannot watch the volume ({status})"),
            }
        }
        for selector in [kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDefaultSystemOutputDevice] {
            if hal::get::<Id>(SYSTEM, selector, GLOBAL) == Some(display) {
                let _ = hal::set(SYSTEM, selector, GLOBAL, &output);
            }
        }
    }

    fn activate(&self, display: Id, output: Id) -> Option<Active> {
        let uid = hal::get_string(display, kAudioDevicePropertyDeviceUID)?;
        let route = Route::new(&uid, TAP_UID).map_err(|status| eprintln!("ddcvolumed: cannot route audio ({status})")).ok()?;
        let ddc = ddc::displays().into_iter().next();
        let vcp = ddc.as_ref().and_then(|d| d.get(ddc::VOLUME).map_err(|e| eprintln!("ddcvolumed: {e}")).ok());
        let (sent, max) = vcp.map_or((0, 0), |v| (v.current, v.max));
        if max > 0 {
            let _ = hal::set(output, kAudioDevicePropertyVolumeScalar, OUTPUT, &(sent as f32 / max as f32));
        }
        Some(Active { display, output, _route: route, ddc, max, sent })
    }

    /// Send the stand-in's volume to the screen.
    fn mirror_volume(&mut self) {
        let Some(active) = &mut self.active else { return };
        let Some(ddc) = &active.ddc else { return };
        let Some(scalar) = hal::get::<f32>(active.output, kAudioDevicePropertyVolumeScalar, OUTPUT) else { return };
        let value = (scalar.clamp(0.0, 1.0) * active.max as f32).round() as u16;
        if value != active.sent {
            match ddc.set(ddc::VOLUME, value) {
                Ok(()) => active.sent = value,
                Err(e) => eprintln!("ddcvolumed: {e}"),
            }
        }
    }
}

fn main() {
    if let Err(status) = hal::notify_on_hal_thread() {
        eprintln!("ddcvolumed: cannot move notifications off the main thread ({status})");
    }
    let (events, received) = channel();
    let watch = |selector, event| {
        let events = events.clone();
        if let Err(status) = hal::listen(SYSTEM, selector, GLOBAL, move || {
            let _ = events.send(event);
        }) {
            eprintln!("ddcvolumed: cannot watch {selector:#x} ({status})");
        }
    };
    watch(kAudioHardwarePropertyDevices, Event::Topology);
    watch(kAudioHardwarePropertyDefaultOutputDevice, Event::Topology);
    watch(kAudioHardwarePropertyDefaultSystemOutputDevice, Event::Topology);
    watch(kAudioHardwarePropertyServiceRestarted, Event::Restarted);

    let mut helper = Helper::default();
    helper.reconcile(&events);
    while let Ok(first) = received.recv() {
        // Coalesce bursts, such as a held volume key, into one pass.
        let batch: Vec<Event> = std::iter::once(first).chain(received.try_iter()).collect();
        if batch.contains(&Event::Restarted) {
            helper = Helper::default();
        }
        if batch.iter().any(|&e| e != Event::Volume) {
            helper.reconcile(&events);
        }
        if batch.contains(&Event::Volume) {
            helper.mirror_volume();
        }
    }
}
