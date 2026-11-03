# ddcvolume

Native volume control on macOS for the speakers of a display connected over
DisplayPort or HDMI.

macOS sends a fixed-level digital audio stream to the display, so the volume
keys, the menu bar slider and the Control Center slider do nothing. ddcvolume
puts them back: they change the display's own volume over DDC/CI, just as its
buttons would.

## How it works

- **`driver`**: a Core Audio server plug-in. It publishes a stand-in output
  device under the display's name. This device has volume and mute controls, so
  macOS treats it like any other output. A hidden tap device plays back what it
  receives.
- **`helper`** (`ddcvolumed`): a per-user agent. It routes the tap to the real
  display through a private aggregate device, which compensates for clock drift.
  It sends every volume change to the display over DDC/CI and switches the
  default output from the display to its stand-in.
- **`ddc`**: DDC/CI over the Apple Silicon display coprocessor (the private
  `IOAVService` API), plus a small CLI:

  ```sh
  ddcvol        # print the volume, e.g. 49/100
  ddcvol 30     # set it
  ddcvol +5     # raise it
  ddcvol -5     # lower it
  ```

## Install

Requirements: Apple Silicon, Rust, and a display with DDC/CI enabled in its
on-screen menu.

```sh
./install.sh            # run as yourself; it asks sudo for the driver
./install.sh uninstall
```

The driver goes to `/Library/Audio/Plug-Ins/HAL`. The helper goes to
`~/Applications/DDCVolume.app` and starts at login. Its log is
`~/Library/Logs/ddcvolumed.log`.

The helper reads the tap as an input, so macOS asks once for microphone access
for "DDCVolume". Without that access, the display stays silent.

## Limitations

- **Volume set on the display itself**: changes made with the display's own
  buttons are not reflected on the Mac.
- **Two entries**: the display appears twice in the output list, because
  macOS cannot hide the original device. Either entry works.
- **One display**: only the first external display is handled.
- **Microphone access after rebuilds**: builds are signed ad hoc, so macOS may
  ask for microphone access again after a rebuild.
