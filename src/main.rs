#![allow(dead_code)]

use crate::lexer::Token;
use ariadne::{Color, Label, Report, ReportKind, Source};
use chumsky::input::Stream;
use chumsky::prelude::*;
use generator::generate;
use logos::Logos;
use parser::luga_parser;

use std::{
    env,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    process::Command,
};

pub use chumsky::span::SimpleSpan;

mod generator;
mod lexer;
mod parser;

fn main() {
    let file_path = PathBuf::from(env::args().nth(1).expect("Expected file argument"));
    let base_name = file_path.file_name().expect("Input must be file");
    let output_file = PathBuf::from(base_name);
    let file_name = base_name.to_string_lossy().to_string();
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

    let Some(program) = result.into_output() else {
        return;
    };

    println!();

    let product = generate(program);

    let Ok(product) = product else {
        eprintln!("Error generating object.");
        return;
    };

    let bytes = product.emit().unwrap();

    let object_file = output_file.with_extension("o");
    let mut f = File::create(&object_file).unwrap();
    f.write_all(&bytes).unwrap();

    println!("Wrote object to {}", object_file.display());

    #[cfg(target_os = "windows")]
    let exe_file = output_file.with_extension("exe");

    #[cfg(not(target_os = "windows"))]
    let exe_file = output_file.with_extension("");

    let status = Command::new("gcc")
        .arg("-o")
        .arg(exe_file)
        .arg(object_file)
        .status()
        .unwrap();

    if !status.success() {
        eprintln!("Error linking executable.");
    }
}
