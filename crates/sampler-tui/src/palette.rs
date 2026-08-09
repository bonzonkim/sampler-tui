use std::path::PathBuf;

use sampler_core::{BankId, ChokeGroup, MidiChannel, MidiChannelFilter, PlaybackMode, Resolution};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEditor {
    text: String,
    cursor: usize,
}

impl LineEditor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn move_left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub fn move_right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.move_left();
        if self.cursor != end {
            self.text.drain(self.cursor..end);
        }
    }

    pub fn delete(&mut self) {
        let Some(character) = self.text[self.cursor..].chars().next() else {
            return;
        };
        self.text
            .drain(self.cursor..self.cursor + character.len_utf8());
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaletteCommand {
    OpenPicker,
    LoadPath(PathBuf),
    Save,
    SaveAs(PathBuf),
    OpenProject(PathBuf),
    Bank(BankId),
    Select(usize),
    StopAll,
    Help,
    Quit,
    Pattern(u8),
    Tempo(f64),
    Bars(u16),
    Resolution(Resolution),
    Swing(f64),
    Quantize(f32),
    Record,
    Play,
    Stop,
    ClearPattern,
    TrimStart(u64),
    TrimEnd(u64),
    Normalize(bool),
    Reverse(bool),
    Pitch(f32),
    Mode(PlaybackMode),
    ApplySample,
    UndoSample,
    Resample,
    RecordInput,
    CaptureStop,
    CaptureCancel,
    PadLevel(f32),
    PadPan(f32),
    PadMute(bool),
    PadChoke(Option<ChokeGroup>),
    DelaySend(f32),
    ReverbSend(f32),
    MasterLevel(f32),
    DelayEnable(bool),
    DelayTime(u16),
    DelayFeedback(f32),
    DelayReturn(f32),
    ReverbEnable(bool),
    ReverbRoom(f32),
    ReverbDamping(f32),
    ReverbReturn(f32),
    MidiPorts,
    MidiConnect(usize),
    MidiDisconnect,
    MidiChannel(MidiChannelFilter),
    MidiLearn,
    MidiUnmap,
    MidiResetBank,
}

pub fn parse_palette(input: &str) -> Result<PaletteCommand, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("enter a command".to_owned());
    }

    let command_end = input.find(char::is_whitespace).unwrap_or(input.len());
    let command = &input[..command_end];
    let remainder = input[command_end..].trim();

    match command {
        "load" if remainder.is_empty() => Ok(PaletteCommand::OpenPicker),
        "load" => Ok(PaletteCommand::LoadPath(PathBuf::from(remainder))),
        "save" => no_arguments(remainder, "save", PaletteCommand::Save),
        "save-as" => parse_project_path(remainder, "save-as").map(PaletteCommand::SaveAs),
        "open-project" => {
            parse_project_path(remainder, "open-project").map(PaletteCommand::OpenProject)
        }
        "bank" => parse_bank(remainder).map(PaletteCommand::Bank),
        "select" => parse_selection(remainder).map(PaletteCommand::Select),
        "stop-all" => no_arguments(remainder, "stop-all", PaletteCommand::StopAll),
        "help" => no_arguments(remainder, "help", PaletteCommand::Help),
        "quit" => no_arguments(remainder, "quit", PaletteCommand::Quit),
        "pattern" => parse_pattern(remainder).map(PaletteCommand::Pattern),
        "tempo" => parse_finite_range(remainder, 20.0, 300.0, "tempo must be 20..300")
            .map(PaletteCommand::Tempo),
        "bars" => parse_bars(remainder).map(PaletteCommand::Bars),
        "resolution" => parse_resolution(remainder).map(PaletteCommand::Resolution),
        "swing" => parse_finite_range(remainder, 50.0, 75.0, "swing must be 50..75")
            .map(|percent| PaletteCommand::Swing(percent / 100.0)),
        "quantize" => parse_finite_range(remainder, 0.0, 100.0, "quantize must be 0..100")
            .map(|percent| PaletteCommand::Quantize((percent / 100.0) as f32)),
        "record" => no_arguments(remainder, "record", PaletteCommand::Record),
        "play" => no_arguments(remainder, "play", PaletteCommand::Play),
        "stop" => no_arguments(remainder, "stop", PaletteCommand::Stop),
        "clear-pattern" => no_arguments(remainder, "clear-pattern", PaletteCommand::ClearPattern),
        "trim-start" => parse_frame(remainder).map(PaletteCommand::TrimStart),
        "trim-end" => parse_frame(remainder).map(PaletteCommand::TrimEnd),
        "normalize" => parse_toggle(remainder, "normalize").map(PaletteCommand::Normalize),
        "reverse" => parse_toggle(remainder, "reverse").map(PaletteCommand::Reverse),
        "pitch" => parse_finite_range(remainder, -24.0, 24.0, "pitch must be -24..24")
            .map(|value| PaletteCommand::Pitch(value as f32)),
        "mode" => parse_mode(remainder).map(PaletteCommand::Mode),
        "apply-sample" => no_arguments(remainder, "apply-sample", PaletteCommand::ApplySample),
        "undo-sample" => no_arguments(remainder, "undo-sample", PaletteCommand::UndoSample),
        "resample" => no_arguments(remainder, "resample", PaletteCommand::Resample),
        "record-input" => no_arguments(remainder, "record-input", PaletteCommand::RecordInput),
        "capture-stop" => no_arguments(remainder, "capture-stop", PaletteCommand::CaptureStop),
        "capture-cancel" => {
            no_arguments(remainder, "capture-cancel", PaletteCommand::CaptureCancel)
        }
        "pad-level" => parse_finite_range(remainder, -60.0, 6.0, "pad-level must be -60..6")
            .map(|value| PaletteCommand::PadLevel(value as f32)),
        "pad-pan" => parse_finite_range(remainder, -1.0, 1.0, "pad-pan must be -1..1")
            .map(|value| PaletteCommand::PadPan(value as f32)),
        "pad-mute" => parse_toggle(remainder, "pad-mute").map(PaletteCommand::PadMute),
        "pad-choke" => parse_choke(remainder).map(PaletteCommand::PadChoke),
        "delay-send" => parse_finite_range(remainder, 0.0, 1.0, "delay-send must be 0..1")
            .map(|value| PaletteCommand::DelaySend(value as f32)),
        "reverb-send" => parse_finite_range(remainder, 0.0, 1.0, "reverb-send must be 0..1")
            .map(|value| PaletteCommand::ReverbSend(value as f32)),
        "master-level" => parse_finite_range(remainder, -60.0, 6.0, "master-level must be -60..6")
            .map(|value| PaletteCommand::MasterLevel(value as f32)),
        "delay-enable" => parse_toggle(remainder, "delay-enable").map(PaletteCommand::DelayEnable),
        "delay-time" => parse_delay_time(remainder).map(PaletteCommand::DelayTime),
        "delay-feedback" => {
            parse_finite_range(remainder, 0.0, 0.95, "delay-feedback must be 0..0.95")
                .map(|value| PaletteCommand::DelayFeedback(value as f32))
        }
        "delay-return" => parse_finite_range(remainder, -60.0, 6.0, "delay-return must be -60..6")
            .map(|value| PaletteCommand::DelayReturn(value as f32)),
        "reverb-enable" => {
            parse_toggle(remainder, "reverb-enable").map(PaletteCommand::ReverbEnable)
        }
        "reverb-room" => parse_finite_range(remainder, 0.0, 1.0, "reverb-room must be 0..1")
            .map(|value| PaletteCommand::ReverbRoom(value as f32)),
        "reverb-damping" => parse_finite_range(remainder, 0.0, 1.0, "reverb-damping must be 0..1")
            .map(|value| PaletteCommand::ReverbDamping(value as f32)),
        "reverb-return" => {
            parse_finite_range(remainder, -60.0, 6.0, "reverb-return must be -60..6")
                .map(|value| PaletteCommand::ReverbReturn(value as f32))
        }
        "midi-ports" => no_arguments(remainder, "midi-ports", PaletteCommand::MidiPorts),
        "midi-connect" => parse_midi_port_index(remainder).map(PaletteCommand::MidiConnect),
        "midi-disconnect" => {
            no_arguments(remainder, "midi-disconnect", PaletteCommand::MidiDisconnect)
        }
        "midi-channel" => parse_midi_channel(remainder).map(PaletteCommand::MidiChannel),
        "midi-learn" => no_arguments(remainder, "midi-learn", PaletteCommand::MidiLearn),
        "midi-unmap" => no_arguments(remainder, "midi-unmap", PaletteCommand::MidiUnmap),
        "midi-reset-bank" => {
            no_arguments(remainder, "midi-reset-bank", PaletteCommand::MidiResetBank)
        }
        _ => Err(format!("unknown command: {command}")),
    }
}

fn parse_midi_port_index(input: &str) -> Result<usize, String> {
    single_token(input, "midi-connect expects a zero-based port index")?
        .parse::<usize>()
        .map_err(|_| "midi-connect expects a zero-based port index".to_owned())
}

fn parse_midi_channel(input: &str) -> Result<MidiChannelFilter, String> {
    let token = single_token(input, "midi-channel must be omni or 1..16")?;
    if token == "omni" {
        return Ok(MidiChannelFilter::Omni);
    }
    token
        .parse::<u8>()
        .ok()
        .and_then(|channel| MidiChannel::new(channel).ok())
        .map(MidiChannelFilter::Channel)
        .ok_or_else(|| "midi-channel must be omni or 1..16".to_owned())
}

fn parse_choke(input: &str) -> Result<Option<ChokeGroup>, String> {
    let token = single_token(input, "pad-choke must be off or 1..16")?;
    if token == "off" {
        return Ok(None);
    }
    token
        .parse::<u8>()
        .ok()
        .and_then(|value| ChokeGroup::new(value).ok())
        .map(Some)
        .ok_or_else(|| "pad-choke must be off or 1..16".to_owned())
}

fn parse_delay_time(input: &str) -> Result<u16, String> {
    single_token(input, "delay-time must be 10..2000")?
        .parse::<u16>()
        .ok()
        .filter(|value| (10..=2_000).contains(value))
        .ok_or_else(|| "delay-time must be 10..2000".to_owned())
}

fn parse_project_path(input: &str, command: &str) -> Result<PathBuf, String> {
    if input.is_empty() {
        return Err(format!("{command} expects a project directory"));
    }
    let mut characters = input.chars();
    let Some(quote @ ('\'' | '"')) = characters.next() else {
        return Ok(PathBuf::from(input));
    };
    let Some(content) = input
        .strip_prefix(quote)
        .and_then(|value| value.strip_suffix(quote))
    else {
        return Err(format!("{command} expects one project directory"));
    };
    if content.is_empty() || content.contains(quote) {
        return Err(format!("{command} expects one project directory"));
    }
    Ok(PathBuf::from(content))
}

fn parse_frame(input: &str) -> Result<u64, String> {
    single_token(input, "frame must be a non-negative integer")?
        .parse::<u64>()
        .map_err(|_| "frame must be a non-negative integer".to_owned())
}

fn parse_toggle(input: &str, name: &str) -> Result<bool, String> {
    match single_token(input, &format!("{name} must be on or off"))? {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("{name} must be on or off")),
    }
}

fn parse_mode(input: &str) -> Result<PlaybackMode, String> {
    match single_token(input, "mode must be oneshot, gate, or loop")? {
        "oneshot" => Ok(PlaybackMode::OneShot),
        "gate" => Ok(PlaybackMode::Gate),
        "loop" => Ok(PlaybackMode::Loop),
        _ => Err("mode must be oneshot, gate, or loop".to_owned()),
    }
}

fn parse_pattern(input: &str) -> Result<u8, String> {
    let value = single_token(input, "pattern must be 1..16")?
        .parse::<u8>()
        .ok()
        .filter(|value| (1..=16).contains(value))
        .ok_or_else(|| "pattern must be 1..16".to_owned())?;
    Ok(value - 1)
}

fn parse_bars(input: &str) -> Result<u16, String> {
    single_token(input, "bars must be 1..64")?
        .parse::<u16>()
        .ok()
        .filter(|value| (1..=64).contains(value))
        .ok_or_else(|| "bars must be 1..64".to_owned())
}

fn parse_resolution(input: &str) -> Result<Resolution, String> {
    match single_token(input, "resolution must be 1/4, 1/8, 1/16, or 1/32")? {
        "1/4" => Ok(Resolution::Quarter),
        "1/8" => Ok(Resolution::Eighth),
        "1/16" => Ok(Resolution::Sixteenth),
        "1/32" => Ok(Resolution::ThirtySecond),
        _ => Err("resolution must be 1/4, 1/8, 1/16, or 1/32".to_owned()),
    }
}

fn parse_finite_range(input: &str, minimum: f64, maximum: f64, error: &str) -> Result<f64, String> {
    let token = single_token(input, error)?;
    let value = token.parse::<f64>().map_err(|_| error.to_owned())?;
    let negative_underflow = minimum >= 0.0
        && value == 0.0
        && token.starts_with('-')
        && token.split(['e', 'E']).next().is_some_and(|significand| {
            significand
                .bytes()
                .any(|digit| matches!(digit, b'1'..=b'9'))
        });
    if value.is_finite() && (minimum..=maximum).contains(&value) && !negative_underflow {
        Ok(value)
    } else {
        Err(error.to_owned())
    }
}

fn single_token<'a>(input: &'a str, error: &str) -> Result<&'a str, String> {
    let mut tokens = input.split_whitespace();
    let Some(token) = tokens.next() else {
        return Err(error.to_owned());
    };
    if tokens.next().is_some() {
        return Err(error.to_owned());
    }
    Ok(token)
}

fn parse_bank(input: &str) -> Result<BankId, String> {
    let mut characters = input.chars();
    let Some(letter) = characters.next() else {
        return Err("bank expects A..=J".to_owned());
    };
    if characters.next().is_some() {
        return Err("bank expects A..=J".to_owned());
    }
    let letter = letter.to_ascii_uppercase();
    if !('A'..='J').contains(&letter) {
        return Err("bank expects A..=J".to_owned());
    }
    let index =
        u8::try_from(u32::from(letter) - u32::from('A')).expect("an ASCII bank index fits in u8");
    BankId::new(index).map_err(|_| "bank expects A..=J".to_owned())
}

fn parse_selection(input: &str) -> Result<usize, String> {
    let value = input
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=16).contains(value))
        .ok_or_else(|| "select expects 1..=16".to_owned())?;
    Ok(value - 1)
}

fn no_arguments(
    remainder: &str,
    name: &str,
    command: PaletteCommand,
) -> Result<PaletteCommand, String> {
    if remainder.is_empty() {
        Ok(command)
    } else {
        Err(format!("{name} does not accept arguments"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sampler_core::{
        BankId, ChokeGroup, MidiChannel, MidiChannelFilter, PlaybackMode, Resolution,
    };

    use super::{LineEditor, PaletteCommand, parse_palette};

    #[test]
    fn editor_inserts_and_deletes_at_the_cursor() {
        let mut editor = LineEditor::default();
        editor.insert('a');
        editor.insert('c');
        editor.move_left();
        editor.insert('b');
        assert_eq!(editor.text(), "abc");
        editor.backspace();
        assert_eq!((editor.text(), editor.cursor()), ("ac", 1));
    }

    #[test]
    fn editor_keeps_its_byte_cursor_on_utf8_boundaries() {
        let mut editor = LineEditor::default();
        for character in "á한b".chars() {
            editor.insert(character);
        }

        editor.move_left();
        editor.move_left();
        assert_eq!(editor.cursor(), 3);
        editor.delete();
        assert_eq!(editor.text(), "áb");
        editor.backspace();
        assert_eq!((editor.text(), editor.cursor()), ("ab", 1));
        editor.move_right();
        editor.move_right();
        editor.delete();
        assert_eq!((editor.text(), editor.cursor()), ("ab", 2));
    }

    #[test]
    fn load_treats_the_trimmed_remainder_as_one_path() {
        assert_eq!(
            parse_palette("load  drums/kick one.wav"),
            Ok(PaletteCommand::LoadPath(PathBuf::from(
                "drums/kick one.wav"
            )))
        );
    }

    #[test]
    fn project_commands_are_strict_and_preserve_directory_remainders() {
        assert_eq!(parse_palette("save"), Ok(PaletteCommand::Save));
        assert_eq!(
            parse_palette("save now"),
            Err("save does not accept arguments".into())
        );
        assert_eq!(
            parse_palette("save-as  projects/live set "),
            Ok(PaletteCommand::SaveAs(PathBuf::from("projects/live set")))
        );
        assert_eq!(
            parse_palette("open-project /Volumes/Sets/Friday Night"),
            Ok(PaletteCommand::OpenProject(PathBuf::from(
                "/Volumes/Sets/Friday Night"
            )))
        );
        assert_eq!(
            parse_palette("save-as"),
            Err("save-as expects a project directory".into())
        );
        assert_eq!(
            parse_palette("open-project"),
            Err("open-project expects a project directory".into())
        );
    }

    #[test]
    fn project_path_commands_accept_one_shell_quoted_remainder() {
        assert_eq!(
            parse_palette("save-as \"projects/live set\""),
            Ok(PaletteCommand::SaveAs(PathBuf::from("projects/live set")))
        );
        assert_eq!(
            parse_palette("open-project 'projects/archive set'"),
            Ok(PaletteCommand::OpenProject(PathBuf::from(
                "projects/archive set"
            )))
        );
        assert_eq!(
            parse_palette("open-project \"projects/live set\" trailing"),
            Err("open-project expects one project directory".into())
        );
        assert_eq!(
            parse_palette("save-as \"unterminated"),
            Err("save-as expects one project directory".into())
        );
    }

    #[test]
    fn commands_are_strict_and_actionable() {
        assert_eq!(
            parse_palette("bank J"),
            Ok(PaletteCommand::Bank(BankId::new(9).unwrap()))
        );
        assert_eq!(parse_palette("select 16"), Ok(PaletteCommand::Select(15)));
        assert_eq!(
            parse_palette("select 0"),
            Err("select expects 1..=16".into())
        );
        assert_eq!(parse_palette("wat"), Err("unknown command: wat".into()));
    }

    #[test]
    fn no_argument_commands_reject_extra_text() {
        assert_eq!(parse_palette("load\t"), Ok(PaletteCommand::OpenPicker));
        assert_eq!(
            parse_palette("help now"),
            Err("help does not accept arguments".into())
        );
        assert_eq!(parse_palette("bank k"), Err("bank expects A..=J".into()));
        assert_eq!(
            parse_palette("SELECT 1"),
            Err("unknown command: SELECT".into())
        );
    }

    #[test]
    fn midi_commands_parse_strict_typed_values() {
        assert_eq!(parse_palette("midi-ports"), Ok(PaletteCommand::MidiPorts));
        assert_eq!(
            parse_palette("midi-connect 0"),
            Ok(PaletteCommand::MidiConnect(0))
        );
        assert_eq!(
            parse_palette("midi-connect 12"),
            Ok(PaletteCommand::MidiConnect(12))
        );
        assert_eq!(
            parse_palette("midi-disconnect"),
            Ok(PaletteCommand::MidiDisconnect)
        );
        assert_eq!(
            parse_palette("midi-channel omni"),
            Ok(PaletteCommand::MidiChannel(MidiChannelFilter::Omni))
        );
        assert_eq!(
            parse_palette("midi-channel 16"),
            Ok(PaletteCommand::MidiChannel(MidiChannelFilter::Channel(
                MidiChannel::new(16).unwrap()
            )))
        );
        assert_eq!(parse_palette("midi-learn"), Ok(PaletteCommand::MidiLearn));
        assert_eq!(parse_palette("midi-unmap"), Ok(PaletteCommand::MidiUnmap));
        assert_eq!(
            parse_palette("midi-reset-bank"),
            Ok(PaletteCommand::MidiResetBank)
        );
    }

    #[test]
    fn midi_commands_reject_missing_extra_and_out_of_range_input() {
        for input in [
            "midi-ports now",
            "midi-connect",
            "midi-connect -1",
            "midi-connect 1.0",
            "midi-connect 9999999999999999999999999999999999999999",
            "midi-connect 1 now",
            "midi-disconnect now",
            "midi-channel",
            "midi-channel 0",
            "midi-channel 17",
            "midi-channel OMNI",
            "midi-channel 1 now",
            "midi-learn now",
            "midi-unmap now",
            "midi-reset-bank now",
        ] {
            assert!(parse_palette(input).is_err(), "accepted invalid {input:?}");
        }
    }

    #[test]
    fn pattern_commands_are_strict_and_ranges_are_typed() {
        assert_eq!(parse_palette("pattern 16"), Ok(PaletteCommand::Pattern(15)));
        assert_eq!(
            parse_palette("tempo 120.5"),
            Ok(PaletteCommand::Tempo(120.5))
        );
        assert_eq!(
            parse_palette("resolution 1/16"),
            Ok(PaletteCommand::Resolution(Resolution::Sixteenth))
        );
        assert_eq!(
            parse_palette("swing 76"),
            Err("swing must be 50..75".into())
        );
        assert_eq!(
            parse_palette("quantize NaN"),
            Err("quantize must be 0..100".into())
        );
    }

    #[test]
    fn pattern_commands_reject_trailing_and_non_finite_values() {
        assert_eq!(
            parse_palette("tempo inf"),
            Err("tempo must be 20..300".into())
        );
        assert_eq!(
            parse_palette("pattern 1 now"),
            Err("pattern must be 1..16".into())
        );
    }

    #[test]
    fn sample_commands_are_typed_and_strict() {
        assert_eq!(
            parse_palette("trim-start 42"),
            Ok(PaletteCommand::TrimStart(42))
        );
        assert_eq!(
            parse_palette("trim-end 99"),
            Ok(PaletteCommand::TrimEnd(99))
        );
        assert_eq!(
            parse_palette("normalize on"),
            Ok(PaletteCommand::Normalize(true))
        );
        assert_eq!(
            parse_palette("reverse off"),
            Ok(PaletteCommand::Reverse(false))
        );
        assert_eq!(
            parse_palette("pitch -12.5"),
            Ok(PaletteCommand::Pitch(-12.5))
        );
        assert_eq!(
            parse_palette("mode loop"),
            Ok(PaletteCommand::Mode(PlaybackMode::Loop))
        );
        assert_eq!(
            parse_palette("apply-sample"),
            Ok(PaletteCommand::ApplySample)
        );
        assert_eq!(parse_palette("undo-sample"), Ok(PaletteCommand::UndoSample));
        assert_eq!(
            parse_palette("pitch NaN"),
            Err("pitch must be -24..24".into())
        );
        assert_eq!(
            parse_palette("mode loop now"),
            Err("mode must be oneshot, gate, or loop".into())
        );
    }

    #[test]
    fn capture_commands_are_no_argument_commands_and_reject_every_trailing_token() {
        for command in ["resample", "record-input", "capture-stop", "capture-cancel"] {
            assert!(
                parse_palette(command).is_ok(),
                "{command} must be recognized"
            );
            for suffix in [" now", " 1", "\t--force", " trailing tokens"] {
                assert_eq!(
                    parse_palette(&format!("{command}{suffix}")),
                    Err(format!("{command} does not accept arguments")),
                    "{command} accepted {suffix:?}",
                );
            }
        }
    }

    #[test]
    fn mixer_commands_parse_every_literal_typed_value() {
        assert_eq!(
            parse_palette("pad-level -12.5"),
            Ok(PaletteCommand::PadLevel(-12.5))
        );
        assert_eq!(
            parse_palette("pad-pan 0.25"),
            Ok(PaletteCommand::PadPan(0.25))
        );
        assert_eq!(
            parse_palette("pad-mute on"),
            Ok(PaletteCommand::PadMute(true))
        );
        assert_eq!(
            parse_palette("pad-choke off"),
            Ok(PaletteCommand::PadChoke(None))
        );
        assert_eq!(
            parse_palette("pad-choke 16"),
            Ok(PaletteCommand::PadChoke(Some(ChokeGroup::new(16).unwrap())))
        );
        assert_eq!(
            parse_palette("delay-send 0.25"),
            Ok(PaletteCommand::DelaySend(0.25))
        );
        assert_eq!(
            parse_palette("reverb-send 0.75"),
            Ok(PaletteCommand::ReverbSend(0.75))
        );
        assert_eq!(
            parse_palette("master-level 6"),
            Ok(PaletteCommand::MasterLevel(6.0))
        );
        assert_eq!(
            parse_palette("delay-enable off"),
            Ok(PaletteCommand::DelayEnable(false))
        );
        assert_eq!(
            parse_palette("delay-time 2000"),
            Ok(PaletteCommand::DelayTime(2000))
        );
        assert_eq!(
            parse_palette("delay-feedback 0.95"),
            Ok(PaletteCommand::DelayFeedback(0.95))
        );
        assert_eq!(
            parse_palette("delay-return -60"),
            Ok(PaletteCommand::DelayReturn(-60.0))
        );
        assert_eq!(
            parse_palette("reverb-enable on"),
            Ok(PaletteCommand::ReverbEnable(true))
        );
        assert_eq!(
            parse_palette("reverb-room 0.5"),
            Ok(PaletteCommand::ReverbRoom(0.5))
        );
        assert_eq!(
            parse_palette("reverb-damping 1"),
            Ok(PaletteCommand::ReverbDamping(1.0))
        );
        assert_eq!(
            parse_palette("reverb-return 6"),
            Ok(PaletteCommand::ReverbReturn(6.0))
        );
    }

    #[test]
    fn mixer_commands_reject_missing_extra_nonfinite_and_out_of_range_values() {
        for input in [
            "pad-level",
            "pad-level 0 extra",
            "pad-level NaN",
            "pad-level -60.1",
            "pad-pan inf",
            "pad-pan 1.01",
            "pad-mute yes",
            "pad-choke 0",
            "pad-choke 17",
            "delay-send -0.01",
            "reverb-send 1.01",
            "master-level 6.1",
            "delay-enable true",
            "delay-time 9",
            "delay-time 2001",
            "delay-time 10.5",
            "delay-feedback 0.96",
            "delay-return -61",
            "reverb-enable",
            "reverb-room NaN",
            "reverb-room 1.01",
            "reverb-damping -0.01",
            "reverb-return inf",
        ] {
            assert!(parse_palette(input).is_err(), "accepted invalid {input:?}");
        }
    }
}
