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

    #[token("=")]
    Equal,

    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,

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

    #[regex(r"[-+]?(?:0|[1-9]\d*)?(?:\.\d+)?(?:[eE][+-]?\d+)?", |lex| lex.slice(), priority = 2)]
    Number(&'a str),

    #[regex(r#"[\"']"#, string)]
    Quoted(String),
}

#[derive(Default, Debug, Clone, PartialEq)]
pub enum LexingError {
    #[default]
    UnexpectedCharacter,
}

fn string<'a>(lex: &mut Lexer<'a, Token<'a>>) -> Result<String, LexingError> {
    let quote = lex.slice().chars().next().unwrap();
    let mut iter = lex.remainder().chars();
    let mut string = String::new();
    while let Some(next_char) = iter.next() {
        if next_char == quote {
            lex.bump(lex.remainder().len() - iter.as_str().len());
            return Ok(string);
        }
        string.push(next_char)
    }
    lex.bump(lex.remainder().len());
    Ok(string)
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
            Token::Comma => write!(f, ","),
            Token::At => write!(f, "@"),
            Token::Id(ident) => write!(f, "{}", ident),
            Token::Number(num) => write!(f, "{}", num),
            Token::Quoted(string) => write!(f, "\"{}\"", string),
            Token::Comment => write!(f, "// ..."),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Equal => write!(f, "="),
        }
    }
}
