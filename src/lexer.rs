use std::fmt::Display;

use logos::{Lexer, Logos, Skip};

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(error = LexingError)]
#[logos(extras = (usize, bool))]
pub enum Token<'a> {
    #[regex("[a-zA-Z]+")]
    Id(&'a str),
    #[token("+", priority = 3)]
    Plus,
    #[token("-", priority = 3)]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,

    #[token("=")]
    Equal,
    #[token("==")]
    EqualEqual,

    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(":")]
    Colon,

    #[token("@")]
    At,

    #[token("(")]
    ParenOpen,
    #[token(")")]
    ParenClose,
    #[token("[")]
    BracketOpen,
    #[token("]")]
    BracketClose,
    #[token("{")]
    BraceOpen,
    #[token("}")]
    BraceClose,

    #[regex(r"\/\/.*[\r\n|\r|\n]", |_| Skip)]
    Comment,

    #[regex(r"[-+]?\d+\.\d+", |lex| lex.slice(), priority = 2)]
    Float(&'a str),

    #[regex(r"[-+]?\d+", |lex| lex.slice(), priority = 2)]
    Integer(&'a str),

    #[regex(r#"\""#, string)]
    Quoted(String),
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum LexingError {
    #[default]
    UnexpectedCharacter,
    UnterminatedString,
}

fn string<'a>(lex: &mut Lexer<'a, Token<'a>>) -> Result<String, LexingError> {
    let mut iter = lex.remainder().chars();
    let mut string = String::new();

    while let Some(next_char) = iter.next() {
        if next_char == '"' {
            lex.bump(lex.remainder().len() - iter.as_str().len());
            return Ok(string);
        }

        if next_char == '\\' {
            match iter.next() {
                Some('n') => string.push('\n'),
                Some('t') => string.push('\t'),
                Some('r') => string.push('\r'),
                Some('\\') => string.push('\\'),
                Some('"') => string.push('"'),
                Some('\'') => string.push('\''),
                Some('0') => string.push('\0'),
                Some(c) => {
                    string.push('\\');
                    string.push(c);
                }
                None => {
                    // Backslash at end of string - this might be an error
                    string.push('\\');
                    break;
                }
            }
        } else if next_char == '\n' {
            break;
        } else {
            string.push(next_char);
        }
    }

    // If we reach here, we hit end of input without finding closing quote
    lex.bump(string.len());
    Err(LexingError::UnterminatedString)
}

impl Display for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::BraceOpen => write!(f, "{{"),
            Token::BraceClose => write!(f, "}}"),
            Token::BracketOpen => write!(f, "["),
            Token::BracketClose => write!(f, "]"),
            Token::ParenOpen => write!(f, "("),
            Token::ParenClose => write!(f, ")"),
            Token::Semicolon => write!(f, ";"),
            Token::Colon => write!(f, ":"),
            Token::Comma => write!(f, ","),
            Token::At => write!(f, "@"),
            Token::Id(ident) => write!(f, "{}", ident),
            Token::Float(num) => write!(f, "{}", num),
            Token::Integer(num) => write!(f, "{}", num),
            Token::Quoted(string) => write!(f, "\"{}\"", string),
            Token::Comment => write!(f, "// ..."),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Slash => write!(f, "/"),
            Token::Equal => write!(f, "="),
            Token::EqualEqual => write!(f, "=="),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logos::Logos;

    #[test]
    fn test_basic_string_lexing() {
        let mut lex = Token::lexer(r#""hello world""#);
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted("hello world".to_string())))
        );
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_empty_string() {
        let mut lex = Token::lexer(r#""""#);
        assert_eq!(lex.next(), Some(Ok(Token::Quoted("".to_string()))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_with_escape_sequences() {
        let mut lex = Token::lexer(r#""hello\nworld\t!""#);
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted("hello\nworld\t!".to_string())))
        );
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_with_all_escape_sequences() {
        let mut lex =
            Token::lexer(r#""newline:\n tab:\t return:\r backslash:\\ quote:\" single:' null:\0""#);
        let expected = "newline:\n tab:\t return:\r backslash:\\ quote:\" single:' null:\0";
        assert_eq!(lex.next(), Some(Ok(Token::Quoted(expected.to_string()))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_with_unknown_escape_sequence() {
        let mut lex = Token::lexer(r#""hello\xworld""#);
        // Unknown escape sequences should preserve the backslash
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted("hello\\xworld".to_string())))
        );
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_with_backslash_at_end() {
        let mut lex = Token::lexer(r#""hello\""#);
        // Backslash at end should be preserved and string should be unterminated
        assert_eq!(lex.next(), Some(Err(LexingError::UnterminatedString)));
    }

    #[test]
    fn test_unterminated_string() {
        let mut lex = Token::lexer(r#""hello world"#);
        assert_eq!(lex.next(), Some(Err(LexingError::UnterminatedString)));
    }

    #[test]
    fn test_unterminated_string_with_escape() {
        let mut lex = Token::lexer(r#""hello\nworld"#);
        assert_eq!(lex.next(), Some(Err(LexingError::UnterminatedString)));
    }

    #[test]
    fn test_string_with_escaped_quote() {
        let mut lex = Token::lexer(r#""He said \"Hello!\" to me""#);
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted("He said \"Hello!\" to me".to_string())))
        );
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_multiple_strings() {
        let mut lex = Token::lexer(r#""first" "second""#);
        assert_eq!(lex.next(), Some(Ok(Token::Quoted("first".to_string()))));
        assert_eq!(lex.next(), Some(Ok(Token::Quoted("second".to_string()))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_with_special_characters() {
        let mut lex = Token::lexer(r#""Special chars: @#$%^&*()[]{}""#);
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted(
                "Special chars: @#$%^&*()[]{}".to_string()
            )))
        );
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_basic_tokens() {
        let mut lex = Token::lexer("+ - * / = ; , @ ( ) [ ] { }");
        let expected_tokens = vec![
            Token::Plus,
            Token::Minus,
            Token::Star,
            Token::Slash,
            Token::Equal,
            Token::Semicolon,
            Token::Comma,
            Token::At,
            Token::ParenOpen,
            Token::ParenClose,
            Token::BracketOpen,
            Token::BracketClose,
            Token::BraceOpen,
            Token::BraceClose,
        ];

        for expected in expected_tokens {
            assert_eq!(lex.next(), Some(Ok(expected)));
        }
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_identifiers() {
        let mut lex = Token::lexer("hello world ABC xyz");
        assert_eq!(lex.next(), Some(Ok(Token::Id("hello"))));
        assert_eq!(lex.next(), Some(Ok(Token::Id("world"))));
        assert_eq!(lex.next(), Some(Ok(Token::Id("ABC"))));
        assert_eq!(lex.next(), Some(Ok(Token::Id("xyz"))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_numbers() {
        let mut lex = Token::lexer("123 -456 +789 3.14 -2.71 +1.0");
        assert_eq!(lex.next(), Some(Ok(Token::Integer("123"))));
        assert_eq!(lex.next(), Some(Ok(Token::Integer("-456"))));
        assert_eq!(lex.next(), Some(Ok(Token::Integer("+789"))));
        assert_eq!(lex.next(), Some(Ok(Token::Float("3.14"))));
        assert_eq!(lex.next(), Some(Ok(Token::Float("-2.71"))));
        assert_eq!(lex.next(), Some(Ok(Token::Float("+1.0"))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_comments() {
        let mut lex = Token::lexer("hello // this is a comment\nworld");
        assert_eq!(lex.next(), Some(Ok(Token::Id("hello"))));
        // Comment should be skipped
        assert_eq!(lex.next(), Some(Ok(Token::Id("world"))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_mixed_content() {
        let mut lex = Token::lexer(r#"func(x, "hello world", 42);"#);
        assert_eq!(lex.next(), Some(Ok(Token::Id("func"))));
        assert_eq!(lex.next(), Some(Ok(Token::ParenOpen)));
        assert_eq!(lex.next(), Some(Ok(Token::Id("x"))));
        assert_eq!(lex.next(), Some(Ok(Token::Comma)));
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted("hello world".to_string())))
        );
        assert_eq!(lex.next(), Some(Ok(Token::Comma)));
        assert_eq!(lex.next(), Some(Ok(Token::Integer("42"))));
        assert_eq!(lex.next(), Some(Ok(Token::ParenClose)));
        assert_eq!(lex.next(), Some(Ok(Token::Semicolon)));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_whitespace_handling() {
        let mut lex = Token::lexer("  hello   \t\n  world  \r\n  ");
        assert_eq!(lex.next(), Some(Ok(Token::Id("hello"))));
        assert_eq!(lex.next(), Some(Ok(Token::Id("world"))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_float_vs_integer_priority() {
        // Test that floats are recognized properly vs integers
        let mut lex = Token::lexer("123.456 123");
        assert_eq!(lex.next(), Some(Ok(Token::Float("123.456"))));
        assert_eq!(lex.next(), Some(Ok(Token::Integer("123"))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_display() {
        let token = Token::Quoted("hello world".to_string());
        assert_eq!(format!("{}", token), "\"hello world\"");
    }

    #[test]
    fn test_complex_string_escaping() {
        let input = r#""Path: C:\\Users\\John\nFile: \"data.txt\"""#;
        let mut lex = Token::lexer(input);
        let expected = "Path: C:\\Users\\John\nFile: \"data.txt\"";
        assert_eq!(lex.next(), Some(Ok(Token::Quoted(expected.to_string()))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_string_with_unicode_like_escape() {
        // Test handling of unknown escape sequences that look like unicode
        let mut lex = Token::lexer(r#""\u1234 \x41""#);
        assert_eq!(
            lex.next(),
            Some(Ok(Token::Quoted("\\u1234 \\x41".to_string())))
        );
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_consecutive_escapes() {
        let mut lex = Token::lexer(r#""\\n\\t\\r""#);
        assert_eq!(lex.next(), Some(Ok(Token::Quoted("\\n\\t\\r".to_string()))));
        assert_eq!(lex.next(), None);
    }

    #[test]
    fn test_error_recovery() {
        // Test that lexer can continue after an error
        let mut lex = Token::lexer("\"unterminated string\nhello");
        assert_eq!(lex.next(), Some(Err(LexingError::UnterminatedString)));
        assert_eq!(lex.next(), Some(Ok(Token::Id("hello"))));
        assert_eq!(lex.next(), None);
    }
}
