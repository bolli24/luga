use crate::lexer::{LexingError, Token};
use crate::parser::{Ast, Spanned, luga_parser};
use ariadne::{Color, Label, Report, ReportKind, Source};
use chumsky::span::SimpleSpan;
use chumsky::{
    Parser,
    input::{Input, Stream},
};
use logos::Logos;

pub enum ParseFailure {
    Lex(Vec<Spanned<LexingError>>),
    Parse(Vec<Spanned<String>>),
}

impl ParseFailure {
    pub fn short_reason(&self) -> String {
        match self {
            Self::Lex(_) => "lexing failed".to_string(),
            Self::Parse(_) => "parsing failed".to_string(),
        }
    }

    pub fn report(&self, file_name: Option<&str>, src: &Source<&str>) {
        match self {
            Self::Lex(errors) => report_errors(file_name, src, errors),
            Self::Parse(errors) => report_errors(file_name, src, errors),
        }
    }
}

pub fn parse_source(src: &str, verbose: bool) -> Result<Ast, ParseFailure> {
    let mut tokens: Vec<(Token, SimpleSpan)> = Vec::new();
    let mut lex_errors: Vec<Spanned<LexingError>> = Vec::new();
    for (tok, span) in Token::lexer(src).spanned() {
        match tok {
            Ok(t) => tokens.push((t, SimpleSpan::from(span))),
            Err(err) => lex_errors.push(Spanned(err, span.into())),
        }
    }

    if verbose {
        for (tok, _) in &tokens {
            println!("{tok:?}");
        }
        println!();
    }

    if !lex_errors.is_empty() {
        return Err(ParseFailure::Lex(lex_errors));
    }

    let token_stream =
        Stream::from_iter(tokens.into_iter()).map((0..src.len()).into(), |(t, s): (_, _)| (t, s));

    let result = luga_parser().parse(token_stream);

    if let Some(program) = result.output()
        && verbose
    {
        for item in &program.items {
            println!("{item:?}");
        }
    }

    result.into_result().map_err(|errors| {
        ParseFailure::Parse(
            errors
                .iter()
                .map(|err| Spanned(err.reason().to_string(), *err.span()))
                .collect(),
        )
    })
}

pub fn report_error<T>(file_name: Option<&str>, src: &Source<&str>, err: &Spanned<T>)
where
    T: ToString + Clone + std::fmt::Debug,
{
    let message = err.0.to_string();
    let range = err.1.into_range();

    if let Some(file_name) = file_name {
        Report::build(ReportKind::Error, (file_name, range.clone()))
            .with_message(message)
            .with_label(Label::new((file_name, range)).with_color(Color::Red))
            .finish()
            .eprint((file_name, src))
            .unwrap();
    } else {
        Report::build(ReportKind::Error, range.clone())
            .with_message(message)
            .with_label(Label::new(range).with_color(Color::Red))
            .finish()
            .eprint(src)
            .unwrap();
    }
}

pub fn report_errors<'a, T>(
    file_name: Option<&str>,
    src: &Source<&str>,
    errors: impl IntoIterator<Item = &'a Spanned<T>>,
) where
    T: 'a + ToString + Clone + std::fmt::Debug,
{
    for err in errors {
        report_error(file_name, src, err);
    }
}
