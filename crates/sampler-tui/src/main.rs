use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sampler_audio::{AudioSession, Frame, PadId, PadSettings, decode_path, prepare_sample};

const USAGE: &str = "Usage:\n  sampler-tui play <path>\n  sampler-tui --help";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const INITIAL_TELEMETRY_TIMEOUT: Duration = Duration::from_secs(1);

type DynError = Box<dyn Error>;

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Play(PathBuf),
    Help,
}

#[derive(Debug, PartialEq, Eq)]
struct InvalidUsage;

fn main() -> ExitCode {
    let action = match parse_args(std::env::args_os()) {
        Ok(action) => action,
        Err(InvalidUsage) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match action {
        Action::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Action::Play(path) => match play(path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                report_error(error.as_ref());
                ExitCode::FAILURE
            }
        },
    }
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Action, InvalidUsage> {
    let Some(_program) = args.next() else {
        return Err(InvalidUsage);
    };
    match (args.next(), args.next(), args.next()) {
        (Some(command), Some(path), None) if command == "play" => {
            Ok(Action::Play(PathBuf::from(path)))
        }
        (Some(command), None, None) if command == "--help" => Ok(Action::Help),
        _ => Err(InvalidUsage),
    }
}

fn play(path: PathBuf) -> Result<(), DynError> {
    let mut session = AudioSession::open_default()?;
    let decoded = decode_path(&path)?;
    let decoded_duration = frame_duration(decoded.frames(), decoded.sample_rate)?;
    let decoded_rate = decoded.sample_rate;
    let sample = prepare_sample(decoded, session.sample_rate())?;
    let sample_frames = u64::try_from(sample.frames())?;
    let playback_duration = frame_duration(sample.frames(), sample.sample_rate())?;

    println!(
        "Output device: {} Hz, {} channel(s)",
        session.sample_rate(),
        session.channels()
    );
    println!(
        "Decoded duration: {:.3} s ({} Hz source)",
        decoded_duration.as_secs_f64(),
        decoded_rate
    );

    session
        .controller_mut()
        .install(PadId::first(), Arc::new(sample), PadSettings::default())?;
    let initial_frame = wait_for_initial_telemetry(&mut session)?;
    let trigger_frame = initial_frame
        .checked_add(128)
        .ok_or_else(|| io::Error::other("audio frame counter overflow before trigger"))?;
    let completion = completion_frame(trigger_frame, sample_frames)
        .ok_or_else(|| io::Error::other("audio frame counter overflow before completion"))?;
    session
        .controller_mut()
        .trigger(PadId::first(), trigger_frame, 1.0)?;

    let deadline = Instant::now() + playback_duration.saturating_add(Duration::from_secs(5));
    loop {
        session.controller_mut().reclaim_retired();
        if let Some(error) = session.poll_error() {
            return Err(Box::new(error));
        }
        if session
            .controller_mut()
            .latest_telemetry()
            .is_some_and(|telemetry| rendered_past_completion(telemetry.rendered_frame, completion))
        {
            return Ok(());
        }
        sleep_until_next_poll(
            deadline,
            "playback timed out before rendered-frame completion",
        )?;
    }
}

fn wait_for_initial_telemetry(session: &mut AudioSession) -> Result<Frame, DynError> {
    let deadline = Instant::now() + INITIAL_TELEMETRY_TIMEOUT;
    loop {
        session.controller_mut().reclaim_retired();
        if let Some(error) = session.poll_error() {
            return Err(Box::new(error));
        }
        if let Some(telemetry) = session.controller_mut().latest_telemetry() {
            return Ok(telemetry.rendered_frame);
        }
        sleep_until_next_poll(deadline, "timed out waiting for initial audio telemetry")?;
    }
}

fn sleep_until_next_poll(deadline: Instant, timeout_message: &'static str) -> Result<(), DynError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::TimedOut,
            timeout_message,
        )));
    }
    thread::sleep(POLL_INTERVAL.min(remaining));
    Ok(())
}

fn frame_duration(frames: usize, sample_rate: u32) -> Result<Duration, DynError> {
    let frames = u64::try_from(frames)?;
    let rate = u64::from(sample_rate);
    if rate == 0 {
        return Err(Box::new(io::Error::other(
            "cannot calculate duration for a zero sample rate",
        )));
    }
    let whole_seconds = frames / rate;
    let fractional_nanos = (frames % rate) * 1_000_000_000 / rate;
    Ok(Duration::from_secs(whole_seconds) + Duration::from_nanos(fractional_nanos))
}

fn completion_frame(trigger_frame: Frame, sample_frames: u64) -> Option<Frame> {
    trigger_frame.checked_add(sample_frames)?.checked_add(64)
}

fn rendered_past_completion(rendered_frame: Frame, completion: Frame) -> bool {
    rendered_frame > completion
}

fn report_error(error: &dyn Error) {
    eprintln!("sampler-tui: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;

    fn arguments(values: &[&str]) -> impl Iterator<Item = OsString> {
        values.iter().map(OsString::from)
    }

    #[test]
    fn play_accepts_exactly_one_os_path_argument() {
        assert_eq!(
            parse_args(arguments(&["sampler-tui", "play", "sample.wav"])),
            Ok(Action::Play(PathBuf::from("sample.wav")))
        );
        assert!(parse_args(arguments(&["sampler-tui", "play"])).is_err());
        assert!(parse_args(arguments(&["sampler-tui", "play", "one.wav", "two.wav"])).is_err());
    }

    #[test]
    fn help_is_the_only_non_play_command() {
        assert_eq!(
            parse_args(arguments(&["sampler-tui", "--help"])),
            Ok(Action::Help)
        );
        assert!(parse_args(arguments(&["sampler-tui"])).is_err());
        assert!(parse_args(arguments(&["sampler-tui", "unknown"])).is_err());
    }

    #[test]
    fn playback_completes_only_after_rendering_past_the_release_tail() {
        let boundary = completion_frame(128, 1).unwrap();
        assert_eq!(boundary, 193);
        assert!(!rendered_past_completion(193, boundary));
        assert!(rendered_past_completion(194, boundary));
    }
}
