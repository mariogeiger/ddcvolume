//! The contract between the driver and the helper that drives it.

/// CFBundleIdentifier of the driver bundle, used to find its plug-in object.
pub const PLUGIN_BUNDLE_ID: &str = "local.ddcvolume.driver";
/// The visible output device that carries the volume control.
pub const OUTPUT_UID: &str = "ddcvolume:output";
/// The hidden input device that plays back what the output device receives.
pub const TAP_UID: &str = "ddcvolume:tap";
/// Plug-in custom property: a CFString naming the display the devices stand
/// in for. The devices exist exactly while it is non-empty.
pub const TARGET_NAME: u32 = u32::from_be_bytes(*b"tgnm");
