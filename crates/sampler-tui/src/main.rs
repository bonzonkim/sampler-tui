use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sampler_audio::{
    AudioSession, Frame, PadId, PadSettings, Telemetry, decode_path, prepare_sample,
};

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

#[derive(Debug, PartialEq, Eq)]
enum CliError {
    DeadlineOverflow,
    Timeout(&'static str),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeadlineOverflow => formatter.write_str("playback deadline is out of range"),
            Self::Timeout(message) => formatter.write_str(message),
        }
    }
}

impl Error for CliError {}

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
    session
        .controller_mut()
        .trigger(PadId::first(), trigger_frame, 1.0)?;

    let deadline = checked_deadline(
        Instant::now(),
        playback_duration.saturating_add(Duration::from_secs(5)),
    )?;
    let mut completion = None;
    loop {
        if let Some(error) = session.poll_error() {
            return Err(Box::new(error));
        }
        ensure_before_deadline(
            Instant::now(),
            deadline,
            "playback timed out before rendered-frame completion",
        )?;
        session.controller_mut().reclaim_retired();
        if let Some(telemetry) = session.controller_mut().latest_telemetry() {
            if completion.is_none() {
                completion = completion_from_trigger_ack(telemetry, sample_frames);
                if completion.is_none() && telemetry.last_triggered_frame.is_some() {
                    return Err(Box::new(io::Error::other(
                        "audio frame counter overflow before completion",
                    )));
                }
            }
            if completion.is_some_and(|completion| {
                rendered_past_completion(telemetry.rendered_frame, completion)
            }) {
                return Ok(());
            }
        }
        sleep_until_next_poll(deadline);
    }
}

fn wait_for_initial_telemetry(session: &mut AudioSession) -> Result<Frame, DynError> {
    let deadline = checked_deadline(Instant::now(), INITIAL_TELEMETRY_TIMEOUT)?;
    loop {
        if let Some(error) = session.poll_error() {
            return Err(Box::new(error));
        }
        ensure_before_deadline(
            Instant::now(),
            deadline,
            "timed out waiting for initial audio telemetry",
        )?;
        session.controller_mut().reclaim_retired();
        if let Some(telemetry) = session.controller_mut().latest_telemetry() {
            return Ok(telemetry.rendered_frame);
        }
        sleep_until_next_poll(deadline);
    }
}

fn checked_deadline(start: Instant, duration: Duration) -> Result<Instant, CliError> {
    start
        .checked_add(duration)
        .ok_or(CliError::DeadlineOverflow)
}

fn ensure_before_deadline(
    now: Instant,
    deadline: Instant,
    timeout_message: &'static str,
) -> Result<(), CliError> {
    if now >= deadline {
        Err(CliError::Timeout(timeout_message))
    } else {
        Ok(())
    }
}

fn sleep_until_next_poll(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        thread::sleep(POLL_INTERVAL.min(remaining));
    }
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

fn completion_from_trigger_ack(telemetry: Telemetry, sample_frames: u64) -> Option<Frame> {
    completion_frame(telemetry.last_triggered_frame?, sample_frames)
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

    use sampler_audio::Telemetry;

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

    #[test]
    fn stale_pre_trigger_telemetry_cannot_supply_a_completion_frame() {
        let telemetry = telemetry(10_000, None);
        assert_eq!(completion_from_trigger_ack(telemetry, 1), None);
    }

    #[test]
    fn a_late_actual_trigger_frame_shifts_the_completion_threshold() {
        let telemetry = telemetry(3_200, Some(1_600));
        assert_eq!(completion_from_trigger_ack(telemetry, 1), Some(1_665));
        assert!(rendered_past_completion(telemetry.rendered_frame, 1_665));
    }

    #[test]
    fn a_deadline_rejects_work_at_the_exact_boundary() {
        let deadline = Instant::now();
        let before = deadline.checked_sub(Duration::from_nanos(1)).unwrap();
        assert!(ensure_before_deadline(before, deadline, "timeout").is_ok());
        assert!(matches!(
            ensure_before_deadline(deadline, deadline, "timeout"),
            Err(CliError::Timeout("timeout"))
        ));
    }

    #[test]
    fn an_unrepresentable_instant_deadline_is_a_typed_error() {
        assert!(matches!(
            checked_deadline(Instant::now(), Duration::MAX),
            Err(CliError::DeadlineOverflow)
        ));
    }

    fn telemetry(rendered_frame: Frame, last_triggered_frame: Option<Frame>) -> Telemetry {
        Telemetry {
            rendered_frame,
            last_triggered_frame,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
        }
    }
}
