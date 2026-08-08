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

Delivery slice 6 project persistence is implemented with automated acceptance evidence: immutable content-addressed audio assets, atomic explicit saves, revisioned recovery saves, staged and failure-atomic project opening, and exact worker-result matching. The command palette supports `save`, `save-as <directory>`, and `open-project <directory>`; paths containing spaces may be entered as the whole remainder or enclosed in matching single or double quotes. Opening another project or quitting resolves an active sample draft first, then offers Save, Discard, or Cancel for modified project state. A newer matching recovery offers Restore, Discard, or Cancel. Interactive crash recovery and filesystem durability across real power loss have not been manually verified.

## Build and test

Rust 1.95.0 is selected by `rust-toolchain.toml`.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo run -p sampler-tui
cargo run -p sampler-tui -- open path/to/project-directory
cargo run -p sampler-tui -- play path/to/sample.wav
```

The no-argument command opens an untitled project in the 80x24-minimum performance TUI. `open <project-directory>` queues that project before interactive input begins. The diagnostic `play` command prints the active device rate/channel count and decoded duration, schedules one one-shot, and exits after rendered-frame completion. A successful exit is programmatic path evidence, not proof that the output was heard or free of audible artifacts. Run `cargo run -p sampler-tui -- --help` for its usage.

The interactive loader is deliberately bounded: a directory view keeps the first 4,096 worker-sorted supported entries and marks a truncated result; encoded inputs above 128 MiB, decoded payloads above 8,388,608 frames or 64 MiB, and prepared output above 8,388,608 frames are rejected with a visible error. These are safety limits, not claims about what a particular codec or audio device can handle below those thresholds.

## Roadmap

1. Add offline mixing, sample decoding/resampling, real device output, and responsive pad triggering.
2. Harden the implemented 80×24 performance TUI with recorded macOS/Linux interactive and hardware acceptance.
3. Record manual cross-platform acceptance for project save/open, recovery, and dirty-quit workflows.
4. Add audio input, resampling, mixer and choke controls, built-in effects, MIDI, export, and cross-platform release hardening.

The approved product design is in [`docs/superpowers/specs/2026-08-07-sampler-tui-design.md`](docs/superpowers/specs/2026-08-07-sampler-tui-design.md), and the slice 1 execution plan is in [`docs/superpowers/plans/2026-08-07-deterministic-sampler-core.md`](docs/superpowers/plans/2026-08-07-deterministic-sampler-core.md).
