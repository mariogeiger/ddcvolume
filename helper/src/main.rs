//! Keep the ddcvolume driver standing in for the connected display:
//! publish its devices under the display's name, route their audio to the
//! display, mirror their volume to the display over DDC/CI, and move the
//! default output from the display to its stand-in.

#![allow(non_upper_case_globals)] // Core Audio's constant names, as in its headers.

#[path = "../../driver/src/contract.rs"]
mod contract;
mod hal;
mod route;

use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::Duration;

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

/// Wait before the first retry of an unsettled state, doubling up to the last.
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(60);

/// The screen's volume over DDC/CI, known only once its range has been read.
struct Screen {
    ddc: ddc::Display,
    max: u16,
    sent: u16,
}

impl Screen {
    /// Read the screen and move the stand-in's slider to its level. This can
    /// fail for a while, as a waking screen answers DDC/CI late.
    fn open(output: Id) -> Result<Screen, String> {
        let ddc = ddc::displays().into_iter().next().ok_or("no display answers DDC/CI")?;
        let vcp = ddc.get(ddc::VOLUME)?;
        if vcp.max == 0 {
            return Err("the screen reports an empty volume range".into());
        }
        let _ = hal::set(output, kAudioDevicePropertyVolumeScalar, OUTPUT, &from_screen(vcp.current, vcp.max));
        Ok(Screen { ddc, max: vcp.max, sent: vcp.current })
    }
}

/// The display being stood in for, while its audio is routed.
struct Active {
    display: Id,
    output: Id,
    _route: Route,
    screen: Option<Screen>,
}

#[derive(Default)]
struct Helper {
    active: Option<Active>,
    listening: Option<Id>,
    /// Delay before the next pass, while the last one left work undone.
    retry: Option<Duration>,
}

/// Steps of the volume keys across the full range.
const KEY_STEPS: f32 = 16.0;

/// Exponent of the loudness curve: the gentlest power law whose first key
/// step is the screen's smallest audible value, 1. Linear (1) when the screen
/// has fewer values than the keys have steps.
fn exponent(max: u16) -> f32 {
    (max as f32).ln().max(KEY_STEPS.ln()) / KEY_STEPS.ln()
}

/// Screen value for a volume scalar; any sound at all is at least 1.
fn to_screen(scalar: f32, max: u16) -> u16 {
    let scalar = scalar.clamp(0.0, 1.0);
    let value = (max as f32 * scalar.powf(exponent(max))).round() as u16;
    if scalar > 0.0 { value.max(1) } else { 0 }
}

/// Volume scalar for a screen value, inverting `to_screen`.
fn from_screen(value: u16, max: u16) -> f32 {
    (value as f32 / max as f32).powf(exponent(max).recip())
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
    /// Report a failure once per unsettled stretch, since retries repeat it.
    fn warn(&self, message: &str) {
        if self.retry.is_none() {
            eprintln!("ddcvolumed: {message}");
        }
    }

    /// Reconcile, and schedule a retry if that left work undone.
    fn pass(&mut self, events: &Sender<Event>) {
        let settled = self.reconcile(events);
        self.retry = (!settled).then(|| self.retry.map_or(FIRST_RETRY, |delay| (delay * 2).min(LAST_RETRY)));
    }

    /// Move towards the desired state; returns whether it has been reached.
    fn reconcile(&mut self, events: &Sender<Event>) -> bool {
        let Some(plugin) = hal::translate(kAudioHardwarePropertyTranslateBundleIDToPlugIn, PLUGIN_BUNDLE_ID) else {
            self.warn(&format!("driver {PLUGIN_BUNDLE_ID} is not loaded"));
            self.active = None;
            return false;
        };
        let display = find_display();
        let name = display.and_then(|d| hal::get_string(d, kAudioObjectPropertyName)).unwrap_or_default();
        if hal::get_string(plugin, TARGET_NAME).as_deref() != Some(&name) {
            // The devices appear asynchronously, and announce themselves as a topology change.
            if let Err(status) = hal::set_string(plugin, TARGET_NAME, &name) {
                self.warn(&format!("cannot name the stand-in ({status})"));
            }
        }
        let output = hal::translate(kAudioHardwarePropertyTranslateUIDToDevice, OUTPUT_UID);
        let tap = hal::translate(kAudioHardwarePropertyTranslateUIDToDevice, TAP_UID);
        let (Some(display), Some(output), Some(_)) = (display, output, tap) else {
            self.active = None;
            return display.is_none();
        };
        if self.active.as_ref().is_none_or(|a| a.display != display || a.output != output) {
            self.active = None;
            self.active = self.activate(display, output);
            if self.active.is_some() {
                eprintln!("ddcvolumed: standing in for {name}");
            }
        }
        if self.active.as_ref().is_some_and(|a| a.screen.is_none()) {
            match Screen::open(output) {
                Ok(screen) => self.active.as_mut().unwrap().screen = Some(screen),
                Err(e) => self.warn(&e),
            }
        }
        if self.listening != Some(output) {
            let events = events.clone();
            match hal::listen(output, kAudioDevicePropertyVolumeScalar, OUTPUT, move || {
                let _ = events.send(Event::Volume);
            }) {
                Ok(()) => self.listening = Some(output),
                Err(status) => self.warn(&format!("cannot watch the volume ({status})")),
            }
        }
        for selector in [kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDefaultSystemOutputDevice] {
            if hal::get::<Id>(SYSTEM, selector, GLOBAL) == Some(display) {
                let _ = hal::set(SYSTEM, selector, GLOBAL, &output);
            }
        }
        self.listening == Some(output) && self.active.as_ref().is_some_and(|a| a.screen.is_some())
    }

    fn activate(&self, display: Id, output: Id) -> Option<Active> {
        let uid = hal::get_string(display, kAudioDevicePropertyDeviceUID)?;
        let route = Route::new(&uid, TAP_UID).map_err(|status| self.warn(&format!("cannot route audio ({status})"))).ok()?;
        Some(Active { display, output, _route: route, screen: None })
    }

    /// Send the stand-in's volume to the screen, or start over on failure.
    fn mirror_volume(&mut self) {
        let Some(active) = &mut self.active else { return };
        let Some(screen) = &mut active.screen else { return };
        let Some(scalar) = hal::get::<f32>(active.output, kAudioDevicePropertyVolumeScalar, OUTPUT) else { return };
        let value = to_screen(scalar, screen.max);
        if value != screen.sent {
            match screen.ddc.set(ddc::VOLUME, value) {
                Ok(()) => screen.sent = value,
                Err(e) => {
                    eprintln!("ddcvolumed: {e}");
                    active.screen = None;
                    self.retry = Some(FIRST_RETRY);
                }
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
    helper.pass(&events);
    loop {
        let batch: Vec<Event> = match received.recv_timeout(helper.retry.unwrap_or(Duration::MAX)) {
            // Coalesce bursts, such as a held volume key, into one pass.
            Ok(first) => std::iter::once(first).chain(received.try_iter()).collect(),
            Err(RecvTimeoutError::Timeout) => vec![Event::Topology],
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if batch.contains(&Event::Restarted) {
            helper = Helper::default();
        }
        if batch.iter().any(|&e| e != Event::Volume) {
            helper.pass(&events);
        }
        if batch.contains(&Event::Volume) {
            helper.mirror_volume();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_key_step_is_one() {
        let steps: Vec<u16> = (0..=16).map(|k| to_screen(k as f32 / KEY_STEPS, 100)).collect();
        assert_eq!(steps[..2], [0, 1]);
        assert_eq!(steps[16], 100);
        assert!(steps.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn screen_values_round_trip() {
        for max in [1, 10, 16, 100, 255] {
            for value in 0..=max {
                assert_eq!(to_screen(from_screen(value, max), max), value, "max {max}");
            }
        }
    }
}
