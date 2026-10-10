# SenseBridge

DualSense haptics and adaptive triggers over Bluetooth, in Windows games that
normally only offer them over a USB cable.

Windows games send DualSense haptics as audio to the controller's USB audio device.
Over Bluetooth that device does not exist, so games fall back to plain rumble or to
nothing. SenseBridge shows Windows a wired DualSense and forwards everything a game
sends to it to your real controller over Bluetooth. No dongle or other hardware.

> Status: early. Tested with one DualSense, one Bluetooth adapter and one game
> (Ghost of Tsushima) on Windows 11. Expect rough edges.

## What works

- High-fidelity haptics, at about the same delay as a cable. Measured from the
  game's audio to the controller moving: about 55 ms through SenseBridge against
  about 80 ms for the same controller on USB.
- Adaptive triggers, light bar, player lights and rumble.
- Buttons, sticks, touchpad and motion, passed through unchanged.
- The controller dropping and coming back mid-game.

## What does not work yet

- The controller's speaker and microphone.
- Settings a game writes with feature reports. No game has needed them so far.
- Hiding the real controller. Games see both it and the virtual one; the ones tried
  picked the virtual one. [HidHide](https://github.com/nefarius/HidHide) can hide the
  real one.
- DualSense Edge is handled in the code but has not been tested.

## Install

1. Install the [usbip-win2](https://github.com/vadimgrn/usbip-win2/releases/latest)
   driver. It is the signed driver that lets a program present a USB device to
   Windows. SenseBridge tells you if it is missing.
2. Download `sensebridge.exe` from the [releases page](https://github.com/siddiqui-zeeshan/sensebridge/releases) and put it
   anywhere. There is no installer.
3. Pair the controller over Bluetooth as usual and run `sensebridge.exe`.

The window is drawn by Microsoft's WebView2 runtime, which Windows 11 includes. On
Windows 10, install it from Microsoft if the app does not open.

Settings are kept in `%APPDATA%\io.github.sensebridge\settings.json`. To keep them
with the program instead, for example on a USB stick, put a `settings.json`
containing `{}` beside `sensebridge.exe`.

## Use

Open SenseBridge and leave it on. When it says **Ready**, start your game.

- **Steam:** turn Steam Input off for the game (Properties, Controller), or Steam
  hands the game a generic gamepad and no haptics are sent.
- **Haptic strength:** 100% sends what the game sent. Use **Test** to feel it.
- **Response:** how much the controller buffers. *Fastest* has the least delay;
  move towards *Smoothest* if haptics stutter.
- Closing the window keeps SenseBridge in the tray. Quit from the tray icon.

When a game starts using the controller, the Bluetooth link can drop for a few
seconds and then return by itself. The cause is not known yet.

## How it works

```
game ──USB audio + HID──▶ virtual wired DualSense ──▶ SenseBridge ──Bluetooth HID──▶ controller
                          (usbip-win2 driver)         (this program)
```

SenseBridge runs a USB/IP server on the loopback interface that emulates a wired
DualSense: the same descriptors, a 4-channel audio device and a HID interface.
usbip-win2 attaches that to Windows as if it were plugged in. From there:

- Audio channels 3 and 4 are the haptics. They are low-pass filtered, resampled
  from 48 kHz to 3 kHz signed 8-bit, and sent as Bluetooth output report `0x32`,
  one every 10.7 ms.
- USB output report `0x02` (triggers, lights, rumble) is re-framed as Bluetooth
  report `0x31` with its CRC.
- Bluetooth input report `0x31` goes back to the game as USB input report `0x01`.

One finding made the latency workable. The haptics report has a field that sets how
much audio the controller queues before playing. Other implementations send 255,
which costs about 195 ms. SenseBridge sends 16, the lowest value that plays cleanly.

## Build

Needs [Rust](https://rustup.rs), the Microsoft C++ build tools and Node.js.

```
cargo test                      # engine and app tests, no controller needed
cargo run -p sensebridge-cli    # headless engine in a terminal
cd app && npm install && npx tauri build   # writes target/release/sensebridge.exe
```

| Path | What |
| --- | --- |
| `engine/` | The bridge: HID access, USB/IP server, emulated device, audio conversion |
| `cli/` | A terminal front end for the engine |
| `app/src-tauri/` | The app: window, tray, settings |
| `app/ui/` | The interface: plain HTML, CSS and JavaScript, no build step |

Open `app/ui/index.html?demo=live` in a browser to work on the interface without a
controller (`off`, `waiting`, `wired`, `driver`, `attaching`, `ready`, `live`, `away`).

## Credits

- [SAxense](https://apps.sdore.me/SAxense) by Egor Vorontsov, which worked out the
  Bluetooth haptics report this is built on.
- [DS5Dongle](https://github.com/awalol/DS5Dongle) by awalol, for the USB
  descriptors and the USB-to-Bluetooth report mapping.
- [usbip-win2](https://github.com/vadimgrn/usbip-win2) by Vadym Hrynchyshyn, the
  driver that makes a user-mode virtual USB device possible on Windows.

SenseBridge is not affiliated with or endorsed by Sony Interactive Entertainment.
DualSense is a trademark of Sony Interactive Entertainment.

## Licence

MIT. See [LICENSE](LICENSE).
