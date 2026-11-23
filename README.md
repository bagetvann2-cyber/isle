# isle

A tiny Dynamic-Island-style pill for Windows: what's playing, and the battery of your wireless gear, in one small black capsule at the top of the screen.

![compact](docs/screenshot-compact.png)
![expanded](docs/screenshot-expanded.png)

## Features

- **Media**: cover, title, artist, progress and play / pause / prev / next for any app that shows up in the Windows media controls (Spotify, Yandex Music, browsers, Media Player...).
- **Waveform**: six bars next to the play control that move with the sound actually coming out of your speakers, tinted with the cover's colour (grey when there is no cover). Switch it off in the menu.
- **Batteries**: wireless headphones, earbuds (left / right / case), mice, keyboards, Xbox controllers. Percent turns green with a bolt while charging, red at 15% or less.
- **Hover** to expand, **drag** to move it, **right-click** (or the tray icon) for the menu.
- **Auto-hide**: invisible and click-through until the cursor comes near it. Hides while a fullscreen app is in front.
- Spring animations, paced to your display refresh and only while something moves.
- Read-only: it shows devices, it never controls them. Only media is controllable.

## Supported devices

| Source | What you get | Status |
|---|---|---|
| Bluetooth Classic / LE (Windows battery property) | one percent per device, no charging info | works for devices Windows reports a battery for |
| Earbud L / R / case over BLE adverts: Redmi / Xiaomi | left, right, case, charging | decoded from a real Redmi Buds 6 capture; L/R order inferred |
| Earbud L / R / case: Google Fast Pair | left, right, case, charging | decoded from the spec, checked against third-party adverts |
| Earbud L / R / case: AirPods | left, right, case, charging | community-documented layout, **unverified** |
| Logitech HID++ 2.0 (Unifying / Bolt / Lightspeed receivers, direct) | percent and charging | experimental, read-only queries, untested on hardware |
| Xbox controllers (Xbox Wireless adapter / Bluetooth / USB) | percent and charging via Windows.Gaming.Input; the XInput fallback only has 4 levels | **untested on hardware** (no Xbox controller available) |
| Razer, SteelSeries, HyperX, Corsair dongles | - | not supported |
| Third-party 2.4 GHz dongles that emulate a wired Xbox 360 pad (e.g. 8BitDo Ultimate 2C) | - | not supported: they report no battery to Windows |

No real mouse has been tested yet: Bluetooth mice should work through the Windows battery property; Logitech is untested.

Earbud parts come from a short passive Bluetooth scan that runs when the island expands (at most every 30 s) and when a headset connects, including at app start. Charging is only shown where the source reports it (Logitech, Xbox controllers, BLE earbuds).

## Resource use

Measured on Windows 11 at 125% scaling:

- idle CPU: about 0% (0 ms over 20 s, visible or hidden, nothing playing or paused)
- waveform, island visible and music playing: about 1% of one core (a bars-only frame costs about 0.2 ms, drawn at about 30 fps, plus a level read about 10 times a second); a silent track that is "playing": about 0.3% (the level is polled every 150 ms); island hidden while music plays: about 0% (under one 16 ms scheduler tick over 20 s); paused: 0 ms
- waveform, live-battery scanning and the progress timer pause while the display is off or the PC is locked
- private memory: about 11-12 MB
- `isle.exe`: about 0.65 MB
- animation frame: about 1.5-2 ms (render + present); no frames and no timers when nothing moves

## Install

Download `isle.exe` from the Releases page and run it. It is unsigned, so Windows SmartScreen will warn: click **More info**, then **Run anyway**. Tested on Windows 11 only (should work on Windows 10 1809+). Antivirus or anti-cheat software may flag an unsigned app that installs a mouse hook (see Privacy).

## Usage

- Move the cursor near the top-center of the screen to reveal it; hover the pill to expand it.
- Click the cover for next track, the play / pause glyph to toggle.
- Drag to reposition (saved); "Reset position" in the menu puts it back.
- Right-click the pill or the tray icon: **Start with Windows**, **Always visible** (turns auto-hide off), **Show waveform**, **Reset position**, **Exit**. Left-clicking the tray icon shows the island for a few seconds.

## Build from source

You need Rust with the `x86_64-pc-windows-gnu` target and MinGW-w64 (for example WinLibs) on `PATH`:

```
rustup default stable-x86_64-pc-windows-gnu
cargo build --release
```

Also run `cargo clippy --release -- -D warnings`, `cargo fmt --check` and `cargo test --release`. Test switches (`ISLE_FAKE_BUDS`, `ISLE_TEST_*`, `ISLE_NOFS`) only work in builds with debug assertions.

## Adding a battery source

A source is one module in `src/battery/` with `fn poll() -> Vec<Device>` (see `bluetooth.rs`). Return an empty list on any error. Call it from the worker in `src/battery/mod.rs`; devices are de-duplicated by `id`. Keep it read-only.

## Privacy

No network access. It reads device batteries, the Windows media session and the output peak level of your default playback device (a single number from the Windows audio meter, used for the waveform; it never records or reads audio), nothing else, and writes nothing to your devices (the Logitech source sends read-only queries; the gamepad source only reads the Windows battery report). Position, the always-visible flag and the waveform flag live in `HKCU\Software\isle`; autostart is the `isle` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

To know when the cursor approaches the island, isle installs a global low-level mouse hook (`WH_MOUSE_LL`). It only compares the cursor position with a small rectangle and posts a message when the cursor enters or leaves it; it reads nothing else and logs nothing. The hook is removed while a fullscreen app is in front and when "Always visible" is on.

Matching a Fast Pair or AirPods advert to your headset is heuristic (signal strength plus agreement with the Windows battery level), so a neighbour's earbuds could in rare cases be shown; Xiaomi / Redmi buds are matched exactly by MAC address.

## Кратко по-русски

Маленькая «капсула» в стиле Dynamic Island для Windows: что сейчас играет и заряд беспроводных устройств (наушники с отдельным зарядом левого/правого/кейса, мышь, клавиатура).

- Запустите `isle.exe` из Releases. Файл не подписан: в SmartScreen нажмите «Подробнее» → «Выполнить в любом случае».
- Капсула прячется и появляется, когда курсор рядом. Наведите курсор, чтобы раскрыть. Перетаскивайте мышью.
- Правый клик по капсуле или значку в трее: «Запускать вместе с Windows», «Всегда показывать», «Показывать волну», «Сбросить позицию», «Выход».
- Чтобы понять, что курсор рядом, используется глобальный хук мыши (только позиция курсора, ничего не логируется). Антивирус или античит могут ругаться на неподписанное приложение с таким хуком.
- Нагрузка: около 0% CPU в покое, около 1% одного ядра при видимой капсуле и играющей музыке (волна), около 11-12 МБ памяти, файл около 0,65 МБ. Сети нет, устройства только читаются. Волна, опрос Bluetooth-наушников и таймер прогресса приостанавливаются, пока экран выключен или ПК заблокирован.
- Сборка: Rust (`x86_64-pc-windows-gnu`) и MinGW-w64, затем `cargo build --release`.

## License

MIT, see [LICENSE](LICENSE).
