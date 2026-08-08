use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "Usage:\n  sampler-tui\n  sampler-tui open <project-directory>\n  sampler-tui play <path>\n  sampler-tui --help";

#[derive(Debug, PartialEq, Eq)]
pub enum CliCommand {
    Tui,
    Open(PathBuf),
    Play(PathBuf),
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUsage;

pub fn parse_args_os(mut args: impl Iterator<Item = OsString>) -> Result<CliCommand, InvalidUsage> {
    let Some(_program) = args.next() else {
        return Err(InvalidUsage);
    };
    match (args.next(), args.next(), args.next()) {
        (None, None, None) => Ok(CliCommand::Tui),
        (Some(command), Some(path), None) if command == "open" => {
            Ok(CliCommand::Open(PathBuf::from(path)))
        }
        (Some(command), Some(path), None) if command == "play" => {
            Ok(CliCommand::Play(PathBuf::from(path)))
        }
        (Some(command), None, None) if command == "--help" => Ok(CliCommand::Help),
        _ => Err(InvalidUsage),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

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
