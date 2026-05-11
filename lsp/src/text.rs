use std::fmt::Display;

use tower_lsp::lsp_types::Position;

#[derive(Debug)]
pub struct Text {
    lines: Vec<String>,
}

impl Text {
    pub fn update(&mut self, start: Position, end: Position, text: &str) {
        assert!(start.line <= end.line, "LSP range start must be before end");

        let start_line_index = start.line as usize;
        let end_line_index = end.line as usize;
        let start_line = self
            .lines
            .get(start_line_index)
            .expect("start line must exist");
        let end_line = self.lines.get(end_line_index).expect("end line must exist");
        let start_byte = utf16_col_to_byte_idx(start_line, start.character);
        let end_byte = utf16_col_to_byte_idx(end_line, end.character);

        if start.line == end.line {
            assert!(
                start.character <= end.character,
                "LSP range start must be before end"
            );
        }

        let prefix = start_line[..start_byte].to_owned();
        let suffix = end_line[end_byte..].to_owned();
        let mut replacement: Vec<String> = logical_lines(text).map(str::to_owned).collect();

        replacement
            .first_mut()
            .expect("logical_lines always yields at least one line")
            .insert_str(0, &prefix);
        replacement
            .last_mut()
            .expect("logical_lines always yields at least one line")
            .push_str(&suffix);

        self.lines
            .splice(start_line_index..=end_line_index, replacement);
    }

    pub fn update_full(&mut self, text: &str) {
        *self = text.into();
    }
}

impl Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for l in &self.lines {
            write!(f, "{l}")?;
        }
        Ok(())
    }
}

impl Into<Text> for &str {
    fn into(self) -> Text {
        Text {
            lines: logical_lines(self).map(|l| l.to_owned()).collect(),
        }
    }
}

fn logical_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .chain((text.is_empty() || text.ends_with('\n')).then_some(""))
}

fn utf16_col_to_byte_idx(line: &str, col: u32) -> usize {
    let line_content = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'));
    let line = line_content.unwrap_or(line);
    let mut utf16_count = 0;

    for (byte_idx, ch) in line.char_indices() {
        if utf16_count == col {
            return byte_idx;
        }

        utf16_count += ch.len_utf16() as u32;

        if utf16_count > col {
            panic!("LSP column is not on a character boundary");
        }
    }

    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn replaces_text_within_single_line() {
        let mut text: Text = "abcdef".into();

        text.update(position(0, 1), position(0, 3), "X");

        assert_eq!(text.to_string(), "aXdef");
    }

    #[test]
    fn inserts_multiple_lines_within_single_line() {
        let mut text: Text = "abcdef".into();

        text.update(position(0, 1), position(0, 3), "X\nY");

        assert_eq!(text.to_string(), "aX\nYdef");
    }

    #[test]
    fn replaces_range_across_lines() {
        let mut text: Text = "abc\ndef\n".into();

        text.update(position(0, 1), position(1, 2), "X");

        assert_eq!(text.to_string(), "aXf\n");
    }

    #[test]
    fn deletes_range_across_lines() {
        let mut text: Text = "abc\ndef\n".into();

        text.update(position(0, 1), position(1, 2), "");

        assert_eq!(text.to_string(), "af\n");
    }

    #[test]
    fn uses_utf16_columns() {
        let mut text: Text = "a😀b".into();

        text.update(position(0, 1), position(0, 3), "X");

        assert_eq!(text.to_string(), "aXb");
    }

    #[test]
    fn inserts_before_line_ending() {
        let mut text: Text = "a\nb".into();

        text.update(position(0, 1), position(0, 1), "X");

        assert_eq!(text.to_string(), "aX\nb");
    }

    #[test]
    fn inserts_before_crlf_line_ending() {
        let mut text: Text = "a\r\nb".into();

        text.update(position(0, 1), position(0, 1), "X");

        assert_eq!(text.to_string(), "aX\r\nb");
    }

    #[test]
    fn clamps_columns_past_line_ending() {
        let mut text: Text = "a\nb".into();

        text.update(position(0, 2), position(0, 2), "X");

        assert_eq!(text.to_string(), "aX\nb");
    }

    #[test]
    fn clamps_columns_past_crlf_line_ending() {
        let mut text: Text = "a\r\nb".into();

        text.update(position(0, 2), position(0, 2), "X");

        assert_eq!(text.to_string(), "aX\r\nb");
    }
}
