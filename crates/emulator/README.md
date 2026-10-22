# Native emulator

`fujimod-emulator` runs catalog hardware models with caller-selected firmware
and inputs. The catalog boot command currently supports **X70 / x70-110**. It
runs the real boot ROM, NAND loader and firmware with strict native device
models. Unsupported hardware stops execution and remains visible in the JSON
report.

```sh
cargo build --locked --release -p fujimod-emulator --features serde \
  --bin fujimod-emulator
target/release/fujimod-emulator boot \
  --configuration x70-110-rec \
  --update /path/to/FPUPDATE.DAT \
  --boot-rom /path/to/bootrom.bin \
  --screenshot /path/to/lcd.png
```

The ID resolves a checked run configuration, which selects the body and
firmware identities and supplies the execution policy and scene inputs. The
selected pair must have a supported native runtime. Use `--overlay ID` to apply
a named variant; repeated overlays are applied in command-line order. Unsupported
start entries or physical inputs are refused during preparation.

Firmware and ROM
size and SHA-256 must match the selected catalog entry. Supplying your own
16 KiB boot ROM dump is required; the command never fetches one. Binary input
paths may be absolute or relative to the current directory.

The default NAND contains the selected update and erased settings space.
The RTC starts at 2000-01-01 and the external image is fixed gray. These are
declared inputs, not camera calibration. First-start language/date pages are
accepted through calibrated MENU/OK ADC presses on the guest timeline. The
report distinguishes normal REC (`system_mode` 1) and first-start REC (3).
It includes pending-operation state and never counts a discovery run as a boot.

Optional persistent inputs:

- `--nand IMAGE`: a committed NAND JSON image exported by the emulator, or a
  complete 128 MiB main-area image (without OOB/spare bytes) whose firmware prefix
  matches the selected update.
  The committed format verifies geometry and firmware base identity.
- `--settings MCU.BIN`: the 16-byte MCU persistent packet, including marker
  byte `0xaa` at offset 14 and an eight-bit zero sum. User settings stored in
  NAND belong in the NAND image.
- `--instruction-limit COUNT`: a positive instruction budget, default two billion.
- `--execution-policy interactive|exact`: execution accounting policy, default
  `interactive`; choose `exact` to stop at instruction boundaries.

The command prints a JSON report and exits 0 at REC, 2 at an instruction limit
or execution frontier, and 1 for invalid inputs or screenshot errors. A requested
PNG captures the currently composed LCD without advancing the guest. Input
files are read only; firmware-written settings remain in the session's medium.

Rust embedding uses `fujimod_emulator::boot::{Options, open, to_rec, screenshot}`;
set `Options.configuration_id` and `Options.overlays` to select the run.
`open` returns an unexecuted `session::Session`; its `board` owns execution and
its other methods supply external inputs and passive observations. `to_rec`
returns a stopped report while retaining the session for inspection. No Python
runtime is required. The Python package exposes the same construction through
`fujimod.emulator.Board.from_inputs`.

## XP5 board and session API

Rust callers can open an XP5 board through
`fujimod_emulator::boot::open_xp5`. `Xp5OpenOptions` supplies a validated CPU
graph, explicit device windows, interrupt state, A-profile setup, resistor-key
options, device resources, and initial memory images. The function builds the
same `board::factory::Graph`, strict device bus and `session::Session` used by
the rest of the emulator. `xp5_device_windows` converts a selected SoC
hardware profile to device windows; callers supply any run-specific device
options and firmware bytes.

For catalog-backed startup facts, `xp5_startup_from_profile` converts a
caller-selected firmware profile and resolved SoC into processor reset state,
GIC initialization, and A-profile timer configuration. It consumes declared
register and hardware facts only; the caller still selects the profile and
provides all firmware memory images.

`xp5_apply_run_scene` maps a resolved application scene's frame timing and
command-image values onto caller-owned XP5 device windows before assembly.

Discovery callers can install explicit AArch64 callee substitutions with
`Board::install_discovery_callee_stubs`. The API rejects strict boards and
reports each requested substitution and its hit count in board state.

Initial memory images must fit declared physical RAM and cannot overlap. The
XP5 open function accepts those bytes without comparing them to a bundled
firmware hash. The caller selects the firmware inputs and loading layout.
