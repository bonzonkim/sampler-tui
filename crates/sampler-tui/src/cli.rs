use std::error::Error;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::{ExportPatternSlot, OfflineExportReceipt, validate_wav_destination};

pub const USAGE: &str = "Usage:\n  sampler-tui\n  sampler-tui open <project-directory>\n  sampler-tui play <path>\n  sampler-tui export <project-directory> <pattern-1..16> <output.wav>\n  sampler-tui --help";

#[derive(Debug, PartialEq, Eq)]
pub enum CliCommand {
    Tui,
    Open(PathBuf),
    Play(PathBuf),
    Export {
        project: PathBuf,
        slot: ExportPatternSlot,
        destination: PathBuf,
    },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUsage;

#[derive(Debug, PartialEq, Eq)]
pub enum CliOutcome {
    Silent,
    Help,
    Export(OfflineExportReceipt),
}

/// Dispatches a parsed command while keeping the headless export path separate from every TUI
/// and diagnostic startup factory.
pub fn dispatch_command<T, P, E>(
    command: CliCommand,
    run_tui: T,
    play: P,
    export: E,
) -> Result<CliOutcome, Box<dyn Error>>
where
    T: FnOnce(Option<PathBuf>) -> Result<(), Box<dyn Error>>,
    P: FnOnce(PathBuf) -> Result<(), Box<dyn Error>>,
    E: FnOnce(PathBuf, ExportPatternSlot, PathBuf) -> Result<OfflineExportReceipt, Box<dyn Error>>,
{
    match command {
        CliCommand::Tui => run_tui(None).map(|()| CliOutcome::Silent),
        CliCommand::Open(directory) => run_tui(Some(directory)).map(|()| CliOutcome::Silent),
        CliCommand::Play(path) => play(path).map(|()| CliOutcome::Silent),
        CliCommand::Export {
            project,
            slot,
            destination,
        } => export(project, slot, destination).map(CliOutcome::Export),
        CliCommand::Help => Ok(CliOutcome::Help),
    }
}

pub fn parse_args_os(mut args: impl Iterator<Item = OsString>) -> Result<CliCommand, InvalidUsage> {
    let Some(_program) = args.next() else {
        return Err(InvalidUsage);
    };
    match args.next() {
        None => Ok(CliCommand::Tui),
        Some(command) if command == "--help" && args.next().is_none() => Ok(CliCommand::Help),
        Some(command) if command == "open" => match (args.next(), args.next()) {
            (Some(path), None) => Ok(CliCommand::Open(PathBuf::from(path))),
            _ => Err(InvalidUsage),
        },
        Some(command) if command == "play" => match (args.next(), args.next()) {
            (Some(path), None) => Ok(CliCommand::Play(PathBuf::from(path))),
            _ => Err(InvalidUsage),
        },
        Some(command) if command == "export" => {
            let (Some(project), Some(slot), Some(destination), None) =
                (args.next(), args.next(), args.next(), args.next())
            else {
                return Err(InvalidUsage);
            };
            let slot = slot
                .to_str()
                .and_then(|value| value.parse::<u8>().ok())
                .ok_or(InvalidUsage)
                .and_then(|value| ExportPatternSlot::try_from(value).map_err(|_| InvalidUsage))?;
            let destination = PathBuf::from(destination);
            validate_wav_destination(&destination).map_err(|_| InvalidUsage)?;
            Ok(CliCommand::Export {
                project: PathBuf::from(project),
                slot,
                destination,
            })
        }
        _ => Err(InvalidUsage),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use crate::export::{ExportPatternSlot, OfflineExportError};

    use super::*;

    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values.iter().map(OsString::from)
    }

    #[test]
    fn no_arguments_select_tui_and_existing_modes_remain() {
        assert_eq!(parse_args_os(args(&["sampler-tui"])), Ok(CliCommand::Tui));
        assert_eq!(
            parse_args_os(args(&["sampler-tui", "--help"])),
            Ok(CliCommand::Help)
        );
        assert_eq!(
            parse_args_os(args(&["sampler-tui", "play", "kick.wav"])),
            Ok(CliCommand::Play(PathBuf::from("kick.wav")))
        );
        assert!(parse_args_os(args(&["sampler-tui", "wat"])).is_err());
    }

    #[test]
    fn play_accepts_exactly_one_os_path_argument() {
        assert!(parse_args_os(args(&["sampler-tui", "play"])).is_err());
        assert!(parse_args_os(args(&["sampler-tui", "play", "one.wav", "two.wav"])).is_err());
    }

    #[test]
    fn open_accepts_exactly_one_project_directory_and_preserves_spaces() {
        assert_eq!(
            parse_args_os(args(&["sampler-tui", "open", "projects/live set"])),
            Ok(CliCommand::Open(PathBuf::from("projects/live set")))
        );
        assert!(parse_args_os(args(&["sampler-tui", "open"])).is_err());
        assert!(parse_args_os(args(&["sampler-tui", "open", "project-a", "project-b"])).is_err());
    }

    #[test]
    fn export_slot_is_strictly_one_based() {
        assert_eq!(ExportPatternSlot::try_from(1).unwrap().slot().get(), 0);
        assert_eq!(ExportPatternSlot::try_from(16).unwrap().slot().get(), 15);
        assert_eq!(
            ExportPatternSlot::try_from(0),
            Err(OfflineExportError::PatternSlot(0))
        );
        assert_eq!(
            ExportPatternSlot::try_from(17),
            Err(OfflineExportError::PatternSlot(17))
        );
    }

    #[test]
    fn export_cli_requires_project_slot_and_wav_destination() {
        assert_eq!(
            parse_args_os(args(&[
                "sampler-tui",
                "export",
                "projects/set",
                "4",
                "mix.wav",
            ])),
            Ok(CliCommand::Export {
                project: PathBuf::from("projects/set"),
                slot: ExportPatternSlot::try_from(4).unwrap(),
                destination: PathBuf::from("mix.wav"),
            })
        );
        assert!(parse_args_os(args(&["sampler-tui", "export", "set", "0", "mix.wav"])).is_err());
        assert!(parse_args_os(args(&["sampler-tui", "export", "set", "1", "mix.flac"])).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn play_preserves_non_unicode_paths() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(vec![b'k', 0x80, b'.', b'w', b'a', b'v']);
        let parsed = parse_args_os(
            [
                OsString::from("sampler-tui"),
                OsString::from("play"),
                path.clone(),
            ]
            .into_iter(),
        );
        assert_eq!(parsed, Ok(CliCommand::Play(PathBuf::from(path))));
    }

    #[cfg(unix)]
    #[test]
    fn open_preserves_non_unicode_project_directories() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(vec![b'p', 0x80, b'r', b'o', b'j']);
        let parsed = parse_args_os(
            [
                OsString::from("sampler-tui"),
                OsString::from("open"),
                path.clone(),
            ]
            .into_iter(),
        );
        assert_eq!(parsed, Ok(CliCommand::Open(PathBuf::from(path))));
    }
}
