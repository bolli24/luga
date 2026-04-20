use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ariadne::{Color, Label, Report, ReportKind, Source};
use chumsky::span::SimpleSpan;
use chumsky::{
    Parser,
    input::{Input, Stream},
};
use logos::Logos;

use crate::lexer::Token;
use crate::parser::{Program, luga_parser};

pub enum ParseFailure {
    Io(PathBuf, io::Error),
    Lex(PathBuf),
    Parse(PathBuf),
}

impl ParseFailure {
    pub fn short_reason(&self) -> String {
        match self {
            Self::Io(_, err) => err.to_string(),
            Self::Lex(_) => "lexing failed".to_string(),
            Self::Parse(_) => "parsing failed".to_string(),
        }
    }
}

pub fn parse_path(path: &Path, verbose: bool) -> Result<(Program, String), ParseFailure> {
    let src = fs::read_to_string(path).map_err(|err| ParseFailure::Io(path.into(), err))?;
    let program = parse_source(path, &src, verbose)?;
    Ok((program, src))
}

pub fn parse_source(path: &Path, src: &str, verbose: bool) -> Result<Program, ParseFailure> {
    let file_name = path.display().to_string();

    let mut tokens: Vec<(Token, SimpleSpan)> = Vec::new();
    let mut lex_errors: Vec<(crate::lexer::LexingError, Range<usize>)> = Vec::new();
    for (tok, span) in Token::lexer(src).spanned() {
        match tok {
            Ok(t) => tokens.push((t, SimpleSpan::from(span))),
            Err(err) => lex_errors.push((err, span)),
        }
    }

    if verbose {
        for (tok, _) in &tokens {
            println!("{tok:?}");
        }
    }

    let src_report = Source::from(src);

    if !lex_errors.is_empty() {
        for (err, span) in &lex_errors {
            report_error(
                &file_name,
                src_report.clone(),
                span.clone(),
                &err.to_string(),
            );
        }
        return Err(ParseFailure::Lex(path.into()));
    }

    let token_stream =
        Stream::from_iter(tokens.into_iter()).map((0..src.len()).into(), |(t, s): (_, _)| (t, s));

    if verbose {
        println!();
    }

    let result = luga_parser().parse(token_stream);

    if let Some(program) = result.output()
        && verbose
    {
        for item in &program.items {
            println!("{item:?}");
        }
    }

    if result.has_errors() {
        for error in result.errors() {
            report_error(
                &file_name,
                src_report.clone(),
                error.span().into_range(),
                &error.to_string(),
            );
        }
    }

    match result.into_output() {
        Some(program) => Ok(program),
        None => Err(ParseFailure::Parse(path.into())),
    }
}

pub fn report_error(file_name: &str, src: Source<&str>, span: Range<usize>, msg: &str) {
    Report::build(ReportKind::Error, (file_name, span.clone()))
        .with_message(msg)
        .with_label(
            Label::new((file_name, span))
                .with_message(msg)
                .with_color(Color::Red),
        )
        .finish()
        .eprint((file_name, src))
        .unwrap();
}
