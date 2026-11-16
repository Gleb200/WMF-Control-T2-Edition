# WMF Control: Windows Mac Fan Control (Apple T2 Edition)

[![Release](https://img.shields.io/badge/release-v1.1.0-blue.svg)](https://github.com/Gleb200/WMF-Control-T2-Edition/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Platform: Windows 10 / 11 (x64)](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011%20(x64)-informational.svg)]()
<img width="900" height="600" alt="image" src="https://github.com/user-attachments/assets/82e84939-35a4-4bb9-be64-a031431e201b" />

**Lightweight, low-overhead fan control and temperature monitor for Intel Macs running Windows 10/11 through Boot Camp.** 

This fork is specifically enhanced and optimized for **Apple T2 chip models** (MacBook Pro 16" 2019, MacBook Pro 13"/15" 2018–2020, MacBook Air 2018–2020, iMac Pro, Mac mini 2018), while remaining fully compatible with older Intel Macs.

> **Original Author:** [melinoe-ux](https://github.com/melinoe-ux/bootcamp-mac-fan-control-windows) · **Fork & T2 Optimizations:** [Gleb200](https://github.com/Gleb200) · **License:** MIT

---

## What's New in T2 Edition (v1.1.0)

* **Dual-Fan T2 Support (Fan 2 0x86 SMC Error Fix):**
  On Apple T2 machines (e.g. MacBookPro16,1), writing to minimum fan speed registers (`F0Mn`, `F1Mn`) is unsupported and returns hardware error `0x86`, which previously prevented Fan 2 from being controlled. T2 Edition completely eliminates `FnMn` writes, cleanly controlling both fans via target speed keys (`F0Tg`, `F1Tg`) in forced mode (`FnMd=1`) with big-endian `fpe2` encoding.
* **Ultra-Low Background CPU Usage (~0.1% – 0.3%):**
  When the app is minimized to the system tray, scanning of all 40+ dormant voltage, power, and secondary temperature keys is stopped. Background polling queries only the active curve sensor, tray indicator (`TCXC` / `TC0P`), emergency thermal guard, and live RPMs (`F0Ac`, `F1Ac`) at a relaxed 2000 ms interval. Full sweeps resume seamlessly whenever the main UI is opened.
* **Battery-Safe Windows Autostart:**
  Standard Windows Task Scheduler jobs created via `schtasks` have `DisallowStartIfOnBatteries=true` by default, causing fan control autostart to fail on laptops running on battery power (stuck in `Queued` state). T2 Edition configures `<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>`, `<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>`, and `<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>`.
* **Zero Console Window / Silent Tray Startup:**
  Compiled as a pure Windows GUI application (`#![windows_subsystem = "windows"]`, no `AllocConsole()`). Supports `--minimized` (or `/tray`) flag to launch directly into the system tray without any black terminal flashes or window flickering.
* **Shared Memory CPU Temp for External Widgets (`Local\WMF_CpuTemp`):**
  Every polling cycle writes the current CPU temperature into a Windows named shared memory section (`Local\WMF_CpuTemp`, 4-byte `f32`). External utilities, Rainmeter skins, and TouchBar tools can read real CPU thermals without needing kernel driver access.
* **Transient MMIO Timeout Caching:**
  Single-cycle bus timeouts or transient MMIO glitches on address `0xFE0B0000` cache and reuse the last valid reading instead of zeroing values out or dropping sensors.
* **Portable & Standard Config Support:**
  Looks for `config.json` next to the executable first (portable mode), falling back to `%APPDATA%\WMF Control\config.json`.

---

## Features

- Live RPM for every fan (current / min / max).
- Three control modes per fan:
  - **Auto**: default Apple SMC hardware control.
  - **Constant RPM**: fixed custom speed.
  - **Sensor-based**: linear temperature ramp between customizable min and max threshold temperatures.
- 40+ temperature, power (W), voltage, and current sensors read directly from SMC keys.
- **Full Blast**: one-click toggle to spin all fans up to 100% max speed.
- **Overheat Guard**: safety threshold automatically boosts all fans to max speed if CPU/GPU reaches 90 °C, handing control back only when cooled below 85 °C.
- **Clean SMC Reset**: switching to Auto or closing the application cleanly restores default Apple SMC automatic fan management (`FnMd=0`).
- °C / °F units, customizable polling interval, and dark-themed interface built with `egui`.

---

## Installation & Quick Start

1. Download **`WMF-Control-T2-v1.1.0-win64.zip`** from [Releases](https://github.com/Gleb200/WMF-Control-T2-Edition/releases).
2. Extract the archive into a folder of your choice (e.g. `C:\Users\<YourUser>\AppData\Local\Programs\WMFControl` or any portable folder).
3. Right-click **`wmf-control.exe`** and select **Run as administrator** (or approve the UAC prompt). The included `inpoutx64.dll` driver provides physical MMIO access to the T2 chip.
4. Configure your desired fan curves (e.g., set both Fan 0 and Fan 1 to **Sensor-based** targeting `TC1C` or `TCXC`).
5. Check **Start with Windows (Task Scheduler, no UAC)** to enable silent autostart on logon.

---

## Command Line Arguments

| Argument | Description |
|---|---|
| `--minimized`, `/tray` | Launch directly into the system tray without showing the main window. |
| `--demo` | Runs on simulated SMC sensor data (no administrator privileges or driver required). |
| `--dump` | Writes all detected SMC keys and raw values to `smc-dump.txt`. |
| `--no-mmio` | Forces legacy port I/O access (for pre-2018 non-T2 Macs). |
| `--selftest` | Performs diagnostic SMC write tests and writes results to `selftest.txt`. |

---

## Hardware Access & Drivers

| Driver | Port I/O | MMIO (Apple T2) |
|---|---|---|
| **inpoutx64.dll** (included) | ✓ | ✓ |
| WinRing0x64.dll | ✓ | ✗ (blocked by modern Windows Defender) |

The SMC on Apple T2 Macs is mapped to physical memory via MMIO (`0xFE0B0000`, discovered via ACPI `APP0001`). `inpoutx64.dll` (by Phil Gibbons, highrez.co.uk) handles physical memory mapping (`MapPhysToLin`).

---

## Building from Source

Prerequisites:
- [Rust toolchain](https://rustup.rs/) (edition 2024 / 1.85+)
- Either MSVC or MinGW toolchain (`x86_64-pc-windows-gnu` / `x86_64-pc-windows-msvc`)

```powershell
# Clone the repository
git clone https://github.com/Gleb200/WMF-Control-T2-Edition.git
cd WMF-Control-T2-Edition

# Build release executable
cargo build --release
```
`build.rs` will automatically embed the required administrator manifest and copy `driver\inpoutx64.dll` next to the compiled `target\release\wmf-control.exe`.

---

## Acknowledgments & License

* **melinoe-ux** for the original [bootcamp-mac-fan-control-windows](https://github.com/melinoe-ux/bootcamp-mac-fan-control-windows) project.
* Phil Gibbons for the **InpOut32/64** driver ([driver/license.txt](driver/license.txt)).
* Linux `applesmc` kernel driver maintainers and the `t2linux` project for hardware research.

Licensed under the [MIT License](LICENSE).
