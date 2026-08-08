use std::path::PathBuf;

use sampler_core::{BankId, Resolution};

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
        _ => Err(format!("unknown command: {command}")),
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
    single_token(input, error)?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (minimum..=maximum).contains(value))
        .ok_or_else(|| error.to_owned())
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

    use sampler_core::{BankId, Resolution};

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
}
