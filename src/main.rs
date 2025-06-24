#![allow(dead_code)]

use crate::lexer::Token;
use ariadne::{Color, Label, Report, ReportKind, Source};
use chumsky::prelude::*;
use chumsky::{Parser, input::Stream};
use generator::{GeneratorErrors, generate};
use logos::Logos;
use parser::{Spanned, luga_parser};
use thiserror::Error;

use std::fs::create_dir_all;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitCode;
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
mod util;

#[derive(clap::Parser, Debug)]
struct Cli {
    #[arg(short, long, default_value_t = false)]
    release: bool,
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(clap::Subcommand, Debug)]
enum CliCommand {
    Build { path: PathBuf },
    Run { path: PathBuf },
    Test,
}

#[derive(Error, Debug)]
enum BuildError {
    #[error("Error lexing file {}", .0.display())]
    Lexer(PathBuf),
    #[error("Error parsing file {}", .0.display())]
    Parser(PathBuf),
    #[error("Error linking file {}", .0.display())]
    Linker(PathBuf),
    #[error("Error generating IR for file {}", .0.display())]
    Generator(PathBuf),
}

fn main() -> ExitCode {
    let cli = <Cli as clap::Parser>::parse();

    let result = match &cli.command {
        CliCommand::Build { path } => build_files(&cli, path),
        CliCommand::Run { path } => run_file(&cli, path),
        CliCommand::Test => todo!(),
    };

    match result {
        Ok(_) => ExitCode::from(0),
        Err(_) => ExitCode::from(1),
    }
}

fn build_files(cli: &Cli, path: &Path) -> Result<PathBuf, ()> {
    if path.is_dir() {
        todo!("Build directories");
    }

    if !path.is_file() {
        panic!("Invalid input path.");
    }

    let output_dir = env::current_dir()
        .expect("Current working directory is invalid.")
        .join("output")
        .join(if cli.release { "release" } else { "debug" });

    create_dir_all(&output_dir).expect("Unable to create output directory.");

    build_file(path, &output_dir, cli.verbose).map_err(|err| {
        eprintln!("Error building executable.");
        eprintln!("{err}");
    })
}

fn run_file(cli: &Cli, path: &Path) -> Result<PathBuf, ()> {
    if path.is_dir() {
        panic!("Can't run more than one file.");
    }

    if !path.is_file() {
        panic!("Invalid input path.");
    }

    let result = build_files(cli, path);
    if let Ok(exe_path) = &result {
        println!("Running executable '{}'", exe_path.display());
        let exe_name = exe_path.file_name().expect("Executable path must be valid");
        let exit_status = Command::new(exe_path)
            .status()
            .expect("Unable to spawn command.");
        if let Some(exit_code) = exit_status.code() {
            if exit_code != 0 {
                eprintln!(
                    "Executable '{}' exited with code '{}'",
                    exe_name.display(),
                    exit_code
                );
            }
        }
        if let Some(signal) = exit_status.signal() {
            eprintln!(
                "Executable '{}' exited with signal '{}",
                exe_name.display(),
                signal
            );
        }
    }
    result
}

fn build_file(file_path: &Path, output_dir: &Path, verbose: bool) -> Result<PathBuf, BuildError> {
    let base_name = file_path.file_name().expect("Input must be file");
    let output_file = output_dir.join(base_name);
    let file_name = base_name.to_string_lossy().to_string();
    let src = fs::read_to_string(file_path).expect("Failed to read file");
    let lexer = Token::lexer(src.as_str());

    if verbose {
        for token in lexer {
            println!("{token:?}");
        }
    }

    let token_iter = Token::lexer(src.as_str())
        .spanned()
        .filter_map(|(tok, span)| match tok {
            Ok(tok) => Some((tok, span.into())),
            _ => None,
        });

    let token_stream =
        Stream::from_iter(token_iter).map((0..src.len()).into(), |(t, s): (_, _)| (t, s));

    if verbose {
        println!();
    }

    let result = luga_parser().parse(token_stream);

    if let Some(program) = result.output() {
        if verbose {
            for statement in &program.statements {
                println!("{statement:?}");
            }
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
        return Err(BuildError::Lexer(PathBuf::from(file_path)));
    };

    if verbose {
        println!();
    }

    let product = match generate(program, verbose) {
        Ok(product) => product,
        Err(errors) => match errors {
            GeneratorErrors::CodeErrors(errors) => {
                let src = Source::from(src.as_str());
                for Spanned(error, span) in errors {
                    Report::build(ReportKind::Error, (&file_name, span.into_range()))
                        .with_message(error.to_string())
                        .with_label(
                            Label::new((&file_name, span.into_range()))
                                .with_message(error.to_string())
                                .with_color(Color::Red),
                        )
                        .finish()
                        .eprint((&file_name, src.clone()))
                        .unwrap();
                }
                return Err(BuildError::Generator(file_path.into()));
            }
            GeneratorErrors::VerifierErrors(verifier_errors) => {
                for error in verifier_errors {
                    // TODO: make this printing nice
                    eprintln!("{error}");
                }
                return Err(BuildError::Generator(file_path.into()));
            }
        },
    };

    let bytes = product.emit().unwrap();

    let object_file = output_file.with_extension("o");
    let mut f = File::create(&object_file).unwrap();
    f.write_all(&bytes).unwrap();

    if verbose {
        println!("Wrote object to {}", object_file.display());
    }

    #[cfg(target_os = "windows")]
    let exe_file = output_file.with_extension("exe");

    #[cfg(not(target_os = "windows"))]
    let exe_file = output_file.with_extension("");

    let status = Command::new("gcc")
        .arg("-o")
        .arg(&exe_file)
        .arg(&object_file)
        .status()
        .unwrap();

    if !status.success() {
        return Err(BuildError::Linker(file_path.into()));
    }

    println!("Saved executable to '{}'", exe_file.display());

    Ok(exe_file)
}
