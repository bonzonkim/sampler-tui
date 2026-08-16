# sampler-tui

`sampler-tui` is an original, keyboard-driven sampler for the terminal, inspired by the immediacy of hardware performance samplers. It supports pad performance, microphone recording, resampling, pattern sequencing, sample editing, effects, MIDI mapping, project persistence, and offline export.

**User guides:** [English](https://bonzonkim.github.io/sampler-tui/) · [한국어](https://bonzonkim.github.io/sampler-tui/ko/)

Both guides cover installation, pad performance, recording, patterns, sample editing, mixing, projects, MIDI, and troubleshooting.

The application currently targets macOS and Linux and requires a terminal of at least 80×24 cells.

## Quick start

Rust 1.95.0 is selected by `rust-toolchain.toml`.

```sh
cargo run -p sampler-tui
```

Other launch modes:

```sh
# Open a saved project in the TUI
cargo run -p sampler-tui -- open path/to/project

# Decode and play one sample as a diagnostic
cargo run -p sampler-tui -- play path/to/sample.wav

# Render one pattern loop without opening audio or terminal devices
cargo run -p sampler-tui -- export path/to/project 1 path/to/output.wav
```

Press `?` in the application for contextual help. Press `:` to enter a command.

## Playing pads

The active bank contains sixteen pads arranged like the keyboard:

```text
1  2  3  4
Q  W  E  R
A  S  D  F
Z  X  C  V
```

- Press a pad key to play it.
- Press an active OneShot pad again to stop it.
- Gate pads stop when their key is released.
- Press `H` while holding Gate pads to latch them. Press a latched pad again to stop it.
- Press `[` or `]` to move between banks A–J.
- Press `Shift+Esc` to stop every pad and the active pattern.
- Press `Shift+1` to toggle Fixed Velocity for MIDI triggers.
- Press `Shift+X` to import a sample into the selected pad. `L` is an alias.

`Space` starts or stops pattern playback; it does not trigger the selected pad.

## Recording a microphone sample

`sampler-tui` records from the operating system's current default input device.

1. Select the destination pad.
2. Press `Ctrl+S`, then `I` to start Record Input. Alternatively enter `:record-input`.
3. Confirm that `PEAK` rises while making sound.
4. Press `Enter` to stop and finalize the take.
5. Press the destination pad key to play the installed sample.

Input is not monitored through the output while recording. Pads and pattern transport remain usable during a take.

Capture controls:

- `Ctrl+S`, then `R`: resample the final stereo master into the selected pad.
- `Ctrl+S`, then `I`: record the default input into the selected pad.
- `Ctrl+S`, then `S`: stop and finalize the active take.
- `Ctrl+S`, then `C`: cancel the active take.
- `Esc`: review whether to discard a recording.

An input capture containing only zero-valued samples is rejected instead of replacing the pad. If the application reports `input capture contains no signal`, verify that the displayed `PEAK` moves and select a working default input device in the operating system.

### macOS microphone note

macOS disables the built-in MacBook microphone while the laptop lid is closed. CoreAudio may still open `MacBook Air Microphone`, but it returns silence. When using the Mac in clamshell mode, select an external microphone, iPhone Continuity microphone, or another working input before starting `sampler-tui`. Restart the application after changing the default input device so the new stream is opened.

The terminal application that launches `sampler-tui` must also have microphone permission in **System Settings → Privacy & Security → Microphone**.

## Patterns, samples, and mixing

The project provides sixteen pattern slots with sample-clock playback, live overdub, step editing, quantization, swing, and next-loop switching. Common pattern controls include:

- `Space`: play or stop the selected pattern.
- `Ctrl+R`: start or stop pattern recording.
- `,` / `.`: select the previous or next pattern.

The sample editor supports trim markers, reverse, normalization, pitch adjustment, and OneShot/Gate/Loop playback modes. Edits are applied to immutable in-memory audio; imported source files are never overwritten.

Each pad has level, pan, mute, choke group, delay send, and reverb send controls. Master controls include level, delay, and reverb. Resampling records the final post-effects master output.

## Projects

Projects store their configuration in TOML and copy committed audio into a content-addressed `audio/` directory. Explicit saves and recovery saves are written atomically. Moving a complete project directory preserves its audio references.

Useful commands:

```text
:save
:save-as path/to/project
:open-project path/to/project
:load path/to/sample.wav
:export path/to/output.wav
```

Paths containing spaces can be entered as the complete command remainder or enclosed in matching single or double quotes. Opening another project or quitting asks how to resolve an unfinished capture, sample edit, or modified project.

## MIDI

MIDI input supports velocity-sensitive pad triggering, channel filtering, per-bank learn mappings, disconnect/reconnect handling, and project persistence.

```text
:midi-ports
:midi-connect 0
:midi-channel omni
:midi-learn
:midi-unmap
:midi-disconnect
```

## Command reference

Enter `:help` or press `?` for the interactive reference. Frequently used commands include:

```text
:load                    :save
:save-as <directory>     :open-project <directory>
:record-input            :resample
:capture-stop            :capture-cancel
:pattern <1..16>         :tempo <20..300>
:record                  :play                    :stop
:apply-sample            :undo-sample
:stop-all                :quit
```

## Workspace

The Rust workspace is split into three crates:

- `sampler-core`: deterministic pad, voice, transport, pattern, effects, and project models without terminal or device dependencies.
- `sampler-audio`: decoding, CoreAudio/host device I/O, real-time mixing, capture, resampling, and effects.
- `sampler-tui`: terminal input and rendering, commands, workers, MIDI, project orchestration, and recovery.

Supported import formats are WAV, AIFF, FLAC, and MP3. The loader caps encoded files at 128 MiB and decoded or prepared audio at 8,388,608 frames or 64 MiB. A directory view retains the first 4,096 sorted supported entries and reports truncation.

## Build and test

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
```

The automated suite covers the real application, worker, controller, engine, persistence, MIDI, and export paths with hermetic audio callbacks. Physical hardware behavior, human-perceived latency, and audibility still require testing on each target machine.

## Documentation site

The bilingual static documentation site lives in `docs/`: English is served from the project root and Korean from `/ko/`. It uses relative asset paths so both locales work locally and under the GitHub Pages project path. To publish it, open the repository's **Settings → Pages**, choose **Deploy from a branch**, then select the `main` branch and `/docs` folder. Future pushes that change `docs/` will update both guides automatically.

## License

MIT
