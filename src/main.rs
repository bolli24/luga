#![allow(dead_code)]

use crate::lexer::Token;
use ariadne::{Color, Label, Report, ReportKind, Source};
use chumsky::input::Stream;
use chumsky::prelude::*;
use logos::Logos;
use parser::luga_parser;

use std::{env, fs};

pub use chumsky::span::SimpleSpan;

mod lexer;
mod parser;

fn main() {
    let file_path = env::args().nth(1).expect("Expected file argument");
    let file_name = std::path::PathBuf::from(&file_path)
        .file_name()
        .expect("Input must be file")
        .to_string_lossy()
        .to_string();
    let src = fs::read_to_string(&file_path).expect("Failed to read file");
    let lexer = Token::lexer(src.as_str());

    for token in lexer {
        println!("{token:?}");
    }

    let token_iter = Token::lexer(src.as_str())
        .spanned()
        .filter_map(|(tok, span)| match tok {
            Ok(tok) => Some((tok, span.into())),
            _ => None,
        });

    let token_stream =
        Stream::from_iter(token_iter).map((0..src.len()).into(), |(t, s): (_, _)| (t, s));

    println!();

    let result = luga_parser().parse(token_stream);

    if let Some(program) = result.output() {
        for statement in &program.statements {
            println!("{statement:?}");
        }
    }

    if result.has_errors() {
        let src = Source::from(src.as_str());
        for error in result.errors() {
            Report::build(ReportKind::Error, (&file_name, error.span().into_range()))
                .with_message(error.to_string())
                .with_label(
                    Label::new((&file_name, error.span().into_range()))
                        .with_message(error.reason())
                        .with_color(Color::Red),
                )
                .finish()
                .eprint((&file_name, src.clone()))
                .unwrap();
        }
    }
}
