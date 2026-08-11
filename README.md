# sampler-tui

`sampler-tui` is an original, keyboard-driven, SP-404-inspired sampler for the terminal. It is designed for immediate pad performance, sample preparation, pattern recording, resampling, and project recall without reproducing Roland branding or proprietary firmware behavior.

The project targets macOS and Linux and is implemented as a Rust workspace:

- `sampler-core` contains deterministic pad, voice, transport, pattern, and project models. It has no terminal or audio-device dependencies.
- `sampler-audio` is the boundary for decoding, device I/O, real-time mixing, recording, and effects.
- `sampler-tui` owns terminal input, rendering, commands, and project orchestration.

## Current status

Delivery slice 1 is complete. The core currently provides:

- ten validated banks with sixteen pads each;
- gate, one-shot, loop, gain, pan, pitch, and choke-group settings;
- fixed-capacity voice allocation with deterministic stealing and no steady-state heap allocation;
- absolute sample-frame transport, loop tracking, musical resolutions, and swing;
- reversible quantization and allocation-free pattern scheduling into caller-owned buffers;
- a versioned TOML project document with portable `audio/` paths and nested validation.

Delivery slice 2 is provisional. The default-device path has been exercised programmatically on macOS through rendered-frame completion, but no human hearing or one-shot-cleanliness verification was performed. Hermetic automated decode tests cover WAV, AIFF, FLAC, and MP3. Linux playback, human audibility, and device-disconnect behavior remain unchecked.

Delivery slice 3 is implemented with automated acceptance evidence. The tests cover the real App/worker-result/render path for loading, triggering, releasing, bank switching, telemetry, and status rendering; bounded rapid sixteen-pad input with typed overflow; 79x23 and 80x24 layouts and all overlays; and ordered audio, keyboard, terminal, and worker cleanup for normal, error, and panic outcomes. Interactive terminal and audio acceptance has not been performed for this slice, so hearing, Linux behavior, physical device loss/retry, keyboard-enhancement compatibility, and manual panic restoration remain unchecked. See [`docs/manual-tui-checklist.md`](docs/manual-tui-checklist.md) for the honest acceptance record.

Delivery slice 4 pattern sequencing is implemented with automated callback-to-TUI workflow evidence: sixteen in-memory pattern slots, sample-clock playback, exact acknowledged live overdub, step edits, quantize/swing, next-loop slot switching, typed pattern/record-ack overflow visibility, and round-robin device-rate rebuild. Pattern state is now included in project saves. Automated tests do not establish human audibility, physical-device behavior, or terminal interaction; see [`docs/manual-pattern-checklist.md`](docs/manual-pattern-checklist.md).

Delivery slice 5 in-memory sample editing is implemented with automated worker-to-audio evidence: exact trim markers, reverse, -1 dBFS normalize, pitch and OneShot/Gate/Loop settings, confirmed Apply, one-level Undo, failure-atomic retry, and device-rate recipe replay. Apply replaces only the pad's immutable in-memory audio; the imported source file is never overwritten. Applied recipes are now included in project saves. Automated tests do not establish human audibility, trim-click quality, physical-device recovery, or interactive marker usability; see [`docs/manual-sample-editor-checklist.md`](docs/manual-sample-editor-checklist.md).

Delivery slice 6 project persistence is implemented with automated acceptance evidence: portable moved project directories, immutable content-addressed `audio/<sha256>.<ext>` assets, atomic explicit saves, separate revisioned recovery saves, staged and failure-atomic project opening, and exact worker-result matching. The first successful save copies committed imports into the project without overwriting the external source. A dirty Sample-editor draft is deliberately excluded until Apply or Discard resolves it. The command palette supports `save`, `save-as <directory>`, and `open-project <directory>`; paths containing spaces may be entered as the whole remainder or enclosed in matching single or double quotes. Opening another project or quitting resolves an active sample draft first, then offers Save, Discard, or Cancel for modified project state. A newer matching recovery offers Restore, Discard, or Cancel. These guarantees cover crashes, I/O failures, symlink substitution, and cooperating serialized workers; they do not claim protection against continuous directory mutation by another process with the same effective filesystem authority. Publication identity is checked immediately after commit, but that check is observational and a residual check-to-final-observation or check-to-cleanup race remains. Interactive crash recovery, filesystem durability across real power loss, long-file responsiveness, and physical queue/device retry remain manually unchecked; see [`docs/manual-project-persistence-checklist.md`](docs/manual-project-persistence-checklist.md).

Delivery slice 7 capture is implemented with automated cross-layer acceptance evidence. The strict no-argument palette commands are:

- `resample` — record the final stereo master into the selected pad;
- `record-input` — record the system default input into the selected pad;
- `capture-stop` — stop and finalize the active take;
- `capture-cancel` — discard the active or finalized-but-uninstalled take.

Press `Ctrl+S` to open the capture command menu, then press `I` for input recording, `R` for resampling, `S` to stop, or `C` to cancel. `Esc` closes the menu without running a command.

SP-404-style performance shortcuts are available from the Perform view:

- `1`–`4`, `Q`–`R`, `A`–`F`, and `Z`–`V` play pads 1–16; `[` and `]` select banks A–J.
- Press an active one-shot pad again to stop it. Gate pads stop on key release.
- Press `H` while holding one or more Gate pads, or hold `H` before pressing them, to keep them playing after release. Press a held pad again to stop it.
- `Shift+1` toggles Fixed Velocity. When enabled, MIDI pad triggers use velocity 127.
- `Shift+X` (pad 14) opens sample import for the selected pad. `L` remains an alias.
- `Shift+Esc` stops every sample and the active pattern. `Space` starts or stops pattern playback.

Record Input does not monitor the input, so captured input is never routed to the output during recording. Recording remains nonblocking for pads, pattern transport, and pattern edits; Enter stops, Escape reviews discard, and Stop All plus held-pad releases remain available through every capture dialog. One take is bounded to 8,388,608 stereo frames, so the displayed maximum duration is derived from the source rate: about 174.76 seconds at 48 kHz or 190.22 seconds at 44.1 kHz. A replacement enters rendered output only after exact worker finalization and audio admission. Save and recovery refuse an unresolved take. After commit, Save As or recovery autosave copies the deterministic WAV into the project's content-addressed `audio/` directory; a moved explicit project and a restarted recovery Restore use that project asset without requiring the runtime-managed temporary source. Quit and Open require an explicit Finalize, Discard, or Cancel choice.

The automated suite uses the real audio engine/controller, input callback adapter, bounded worker, project store, and App workflow, but it does not open capture hardware or establish audibility. System input/output device selection and input monitoring remain deferred; offline pattern export is covered separately below. All macOS and Linux hardware/hearing rows remain unchecked in [`docs/manual-capture-checklist.md`](docs/manual-capture-checklist.md).

Delivery slice 8 Mixer/FX and choke control is implemented with automated cross-layer evidence. Each pad has mute, a 1–16/off choke group, and independent delay/reverb sends; the master has level plus bounded delay and reverb parameters. Changes are available from the Mixer workspace and strict palette commands, ramp over 64 rendered frames, persist in schema v3 explicit and recovery saves, and default exactly dry when a schema v2 project is opened. Resample records the exact post-master wet frames and installs them transactionally. Automated evidence covers deterministic dry/wet rendering, active polyphonic ramps, same-group choke release, queue/open/device-failure rollback, portable Save As/open, recovery Restore, exact legacy defaults, Unicode-safe 80×24 child layouts, and callback allocation gates. It does not establish physical audibility, live-control feel, effect-tail quality, or device-loss behavior on real hardware; every macOS and Linux row remains unchecked in [`docs/manual-mixer-fx-checklist.md`](docs/manual-mixer-fx-checklist.md).

Delivery slice 9 MIDI input and mapping has automated cross-layer evidence through virtual ingress, the real App/controller/engine, worker, project store, and filesystem. The suite covers discovery and explicit connection, velocity-sensitive triggering, exact engine-acknowledged Gate recording, trigger-time ownership across bank changes, distinct per-bank Learn maps, bounded overflow quarantine and held-note release, disappearance/reconnect, portable Save As/open, recovery Restore, schema v3 default migration, bitwise-stable dry rendering, and callback allocation bounds. These are automated claims only: physical USB/virtual-port behavior, perceived latency and feel, audibility, long-session hardware behavior, and terminal restoration remain unchecked on macOS and Linux in [`docs/manual-midi-checklist.md`](docs/manual-midi-checklist.md).

Offline pattern export has automated evidence for exactly these guarantees:

- a canonical stereo IEEE 32-bit float WAV at 48 kHz containing exactly one pattern loop;
- bitwise decoded-frame parity with an independently bootstrapped production audio engine;
- bounded worker progress and continued App, pad, and MIDI responsiveness while export runs;
- create-new, no-replace publication that never overwrites an existing destination;
- headless export without terminal, keyboard, MIDI, audio-input, or audio-output device initialization.

Use `export <path>` in the command palette for the selected pattern, or `sampler-tui export <project-directory> <pattern-1..16> <output.wav>` for headless operation. Physical DAW import, hearing, long cancellation, real slow/removable filesystems, OS error behavior, and interactive terminal restoration are not automated claims; every macOS and Linux row remains unchecked in [`docs/manual-offline-export-checklist.md`](docs/manual-offline-export-checklist.md).

## Build and test

Rust 1.95.0 is selected by `rust-toolchain.toml`.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo run -p sampler-tui
cargo run -p sampler-tui -- open path/to/project-directory
cargo run -p sampler-tui -- play path/to/sample.wav
cargo run -p sampler-tui -- export path/to/project-directory 1 path/to/output.wav
```

The no-argument command opens an untitled project in the 80x24-minimum performance TUI. `open <project-directory>` queues that project before interactive input begins. The diagnostic `play` command prints the active device rate/channel count and decoded duration, schedules one one-shot, and exits after rendered-frame completion. A successful exit is programmatic path evidence, not proof that the output was heard or free of audible artifacts. Run `cargo run -p sampler-tui -- --help` for its usage.

The interactive loader is deliberately bounded: a directory view keeps the first 4,096 worker-sorted supported entries and marks a truncated result; encoded inputs above 128 MiB, decoded payloads above 8,388,608 frames or 64 MiB, and prepared output above 8,388,608 frames are rejected with a visible error. These are safety limits, not claims about what a particular codec or audio device can handle below those thresholds.

## Roadmap

1. Add offline mixing, sample decoding/resampling, real device output, and responsive pad triggering.
2. Harden the implemented 80×24 performance TUI with recorded macOS/Linux interactive and hardware acceptance.
3. Record manual cross-platform acceptance for project save/open, recovery, and dirty-quit workflows.
4. Record cross-platform export acceptance, add device selection and optional input monitoring, and continue release hardening.

The approved product design is in [`docs/superpowers/specs/2026-08-07-sampler-tui-design.md`](docs/superpowers/specs/2026-08-07-sampler-tui-design.md), and the slice 1 execution plan is in [`docs/superpowers/plans/2026-08-07-deterministic-sampler-core.md`](docs/superpowers/plans/2026-08-07-deterministic-sampler-core.md).
