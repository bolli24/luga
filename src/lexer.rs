use std::fmt::Display;

use logos::{Logos, Skip};

#[derive(Logos, Debug, Clone, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(error = LexingError)]
#[logos(extras = (usize, bool))]
pub enum Token<'a> {
    #[regex("[a-zA-Z][a-zA-Z0-9_]*")]
    Id(&'a str),
    #[token("+", priority = 3)]
    Plus,
    #[token("-", priority = 3)]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("!")]
    Bang,

    #[token("=")]
    Equal,
    #[token("==")]
    EqualEqual,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,

    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(":")]
    Colon,
    #[token(".")]
    Dot,

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

    #[regex(r"//[^\r\n]*", |_| Skip, allow_greedy = true)]
    Comment,

    #[regex(r"[-+]?\d+\.\d+", |lex| lex.slice(), priority = 2)]
    Float(&'a str),

    #[regex(r"[-+]?\d+", |lex| lex.slice(), priority = 2)]
    Integer(&'a str),

    #[regex(r#""(?:[^"\\\n]|\\.)*""#, |lex| {
        let s = lex.slice();
        Ok::<String, LexingError>(process_escapes(&s[1..s.len() - 1]))
    })]
    #[regex(r#""(?:[^"\\\n]|\\.)*"#, |_| Err(LexingError::UnterminatedString))]
    Quoted(String),
}

#[derive(Default, Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LexingError {
    #[default]
    #[error("Unexpected character")]
    UnexpectedCharacter,
    #[error("Unterminated string literal")]
    UnterminatedString,
}

fn process_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('t') => result.push('\t'),
            Some('r') => result.push('\r'),
            Some('\\') => result.push('\\'),
            Some('"') => result.push('"'),
            Some('\'') => result.push('\''),
            Some('0') => result.push('\0'),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }
    result
}

impl Display for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BraceOpen => write!(f, "{{"),
            Self::BraceClose => write!(f, "}}"),
            Self::BracketOpen => write!(f, "["),
            Self::BracketClose => write!(f, "]"),
            Self::ParenOpen => write!(f, "("),
            Self::ParenClose => write!(f, ")"),
            Self::Semicolon => write!(f, ";"),
            Self::Comma => write!(f, ","),
            Self::Colon => write!(f, ":"),
            Self::Dot => write!(f, "."),
            Self::At => write!(f, "@"),
            Self::Id(ident) => write!(f, "{ident}"),
            Self::Float(num) => write!(f, "{num}"),
            Self::Integer(num) => write!(f, "{num}"),
            Self::Quoted(string) => write!(f, "\"{string}\""),
            Self::Comment => write!(f, "// ..."),
            Self::Plus => write!(f, "+"),
            Self::Minus => write!(f, "-"),
            Self::Star => write!(f, "*"),
            Self::Slash => write!(f, "/"),
            Self::Bang => write!(f, "!"),
            Self::Equal => write!(f, "="),
            Self::EqualEqual => write!(f, "=="),
            Self::Lt => write!(f, "<"),
            Self::Gt => write!(f, ">"),
            Self::LtEq => write!(f, "<="),
            Self::GtEq => write!(f, ">="),
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
        let mut lex = Token::lexer("+ - * / ! = ; , @ ( ) [ ] { }");
        let expected_tokens = vec![
            Token::Plus,
            Token::Minus,
            Token::Star,
            Token::Slash,
            Token::Bang,
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
        assert_eq!(format!("{token}"), "\"hello world\"");
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
