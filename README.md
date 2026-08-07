# sampler-tui

`sampler-tui` is an original, keyboard-driven, SP-404-inspired sampler for the terminal. It is designed for immediate pad performance, sample preparation, pattern recording, resampling, and project recall without reproducing Roland branding or proprietary firmware behavior.

The project targets macOS and Linux and is implemented as a Rust workspace:

- `sampler-core` contains deterministic pad, voice, transport, pattern, and project models. It has no terminal or audio-device dependencies.
- `sampler-audio` is the boundary for decoding, device I/O, real-time mixing, recording, and effects.
- `sampler-tui` owns terminal input, rendering, commands, and project orchestration.

## Current status

Delivery slice 1 is complete on the feature branch. The core currently provides:

- ten validated banks with sixteen pads each;
- gate, one-shot, loop, gain, pan, pitch, and choke-group settings;
- fixed-capacity voice allocation with deterministic stealing and no steady-state heap allocation;
- absolute sample-frame transport, loop tracking, musical resolutions, and swing;
- reversible quantization and allocation-free pattern scheduling into caller-owned buffers;
- a versioned TOML project document with portable `audio/` paths and nested validation.

No audio is emitted yet. The executable is a workspace-boundary smoke test until delivery slice 2 connects decoding and a real output device.

## Build and test

Rust 1.95.0 is selected by `rust-toolchain.toml`.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo run -p sampler-tui
```

The binary currently prints `sampler-tui: core ready`.

## Roadmap

1. Add offline mixing, sample decoding/resampling, real device output, and responsive pad triggering.
2. Build the 80×24 minimum performance TUI, keyboard mapping, help, command palette, file picker, meters, and terminal restoration.
3. Add waveform sample editing, pattern recording/editing, project save/load, atomic saves, and autosave recovery.
4. Add audio input, resampling, mixer and choke controls, built-in effects, MIDI, export, and cross-platform release hardening.

The approved product design is in [`docs/superpowers/specs/2026-08-07-sampler-tui-design.md`](docs/superpowers/specs/2026-08-07-sampler-tui-design.md), and the slice 1 execution plan is in [`docs/superpowers/plans/2026-08-07-deterministic-sampler-core.md`](docs/superpowers/plans/2026-08-07-deterministic-sampler-core.md).
