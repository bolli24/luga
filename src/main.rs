#![allow(dead_code)]

use ariadne::Source;
use luga::GeneratorErrors;
use luga::errors::report_compile_errors;
use luga::generator::generate;
use luga_syntax::parsing::{ParseFailure, parse_source};
use thiserror::Error;

use colored::Colorize;
use std::fs::{self, File, create_dir_all, remove_dir_all};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{env, io};

#[derive(clap::Parser)]
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
    Build {
        #[arg(short, long, default_value_t = String::new())]
        linker_args: String,
        path: PathBuf,
    },
    Run {
        #[arg(short, long, default_value_t = String::new())]
        linker_args: String,
        path: PathBuf,
    },
    Test {
        #[arg(default_value = "test")]
        path: PathBuf,
    },
    Clean,
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
    #[error("I/O error for file {}: {}", .0.display(), .1)]
    Io(PathBuf, io::Error),
}

fn main() -> ExitCode {
    let cli = <Cli as clap::Parser>::parse();

    let result = match &cli.command {
        CliCommand::Build { path, linker_args } => build_files(&cli, path, linker_args).map(|_| ()),
        CliCommand::Run { path, linker_args } => run_file(&cli, path, linker_args),
        CliCommand::Test { path } => test_files(&cli, path),
        CliCommand::Clean => clean(),
    };

    match result {
        Ok(()) => ExitCode::from(0),
        Err(()) => ExitCode::from(1),
    }
}

const OUTPUT_DIR: &str = "output";

fn output_dir(release: bool) -> io::Result<PathBuf> {
    let dir = env::current_dir()?
        .join(OUTPUT_DIR)
        .join(if release { "release" } else { "debug" });
    create_dir_all(&dir)?;
    Ok(dir)
}

fn build_files(cli: &Cli, path: &Path, linker_args: &str) -> Result<PathBuf, ()> {
    let runtime_lib = extract_runtime_lib().map_err(|err| {
        eprintln!("ERROR: Could not extract runtime library.");
        eprintln!("{err}");
    })?;

    if path.is_dir() {
        todo!("Build directories");
    }

    if !path.is_file() {
        eprintln!("ERROR: '{}' is not a file.", path.display());
        return Err(());
    }

    let output_dir = output_dir(cli.release).map_err(|err| {
        eprintln!("ERROR: Unable to create output directory: {err}");
    })?;

    build_file(
        path,
        &output_dir,
        cli.verbose,
        cli.release,
        linker_args,
        &runtime_lib,
    )
    .map_err(|err| {
        eprintln!("ERROR: Could not build executable.");
        eprintln!("{err}");
    })
}

struct TestDirective {
    skip: bool,
    expected_exit_code: i32,
}

fn parse_test_directive(line: &str) -> Result<TestDirective, &'static str> {
    let mut directive = TestDirective {
        skip: false,
        expected_exit_code: 0,
    };

    let Some(rest) = line.strip_prefix("///") else {
        return Ok(directive);
    };

    for token in rest.split_whitespace() {
        if token == "SKIP_TEST" {
            directive.skip = true;
        } else if let Some(value) = token.strip_prefix("EXIT_CODE=") {
            directive.expected_exit_code =
                value.parse().map_err(|_| "invalid EXIT_CODE directive")?;
        }
    }

    Ok(directive)
}

fn test_files(cli: &Cli, path: &Path) -> Result<(), ()> {
    let files: Vec<PathBuf> = if path.is_file() {
        vec![path.to_path_buf()]
    } else if path.is_dir() {
        let mut entries: Vec<PathBuf> = walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "luga"))
            .collect();
        entries.sort();
        entries
    } else {
        eprintln!("ERROR: '{}' is not a file or directory.", path.display());
        return Err(());
    };

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut skipped = 0u32;
    let mut build_failed = 0u32;

    for file in &files {
        let display = file.display();
        if cli.verbose {
            println!("{display} ...");
        } else {
            print!("{display} ... ");
            let _ = io::stdout().flush();
        }

        let content = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(err) => {
                println!("{} (unable to read: {err})", "BUILD FAIL".yellow());
                build_failed += 1;
                continue;
            }
        };

        let first_line = content.lines().next().unwrap_or("");
        let directive = match parse_test_directive(first_line) {
            Ok(d) => d,
            Err(msg) => {
                println!("{} ({msg})", "BUILD FAIL".yellow());
                build_failed += 1;
                continue;
            }
        };

        if directive.skip {
            println!("{}", "SKIP".cyan());
            skipped += 1;
            continue;
        }

        let exe_path = match build_files(cli, file, "") {
            Ok(p) => p,
            Err(()) => {
                println!("{}", "BUILD FAIL".yellow());
                build_failed += 1;
                continue;
            }
        };

        let mut cmd = Command::new(&exe_path);
        if !cli.verbose {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
        let status = match cmd.status() {
            Ok(s) => s,
            Err(err) => {
                println!("{} (unable to execute: {err})", "FAIL".red());
                failed += 1;
                continue;
            }
        };
        if cli.verbose {
            println!();
        }

        let expected = directive.expected_exit_code;
        match status.code() {
            Some(code) if code == expected => {
                println!("{}", "PASS".green());
                passed += 1;
            }
            Some(code) => {
                println!(
                    "{} (expected exit code {expected}, got {code})",
                    "FAIL".red()
                );
                failed += 1;
            }
            None => {
                println!("{} (terminated without exit code)", "FAIL".red());
                failed += 1;
            }
        }
    }

    let total = passed + failed + skipped + build_failed;
    println!();
    println!(
        "Test summary: {} / {} / {} total | Skipped: {skipped} | Build failed: {}",
        format!("{failed} failed").red(),
        format!("{passed} passed").green(),
        total,
        format!("{build_failed}").yellow(),
    );

    if failed > 0 || build_failed > 0 {
        Err(())
    } else {
        Ok(())
    }
}

fn run_file(cli: &Cli, path: &Path, linker_args: &str) -> Result<(), ()> {
    if path.is_dir() {
        eprintln!("ERROR: Can't run more than one file.");
        return Err(());
    }
    if !path.is_file() {
        eprintln!("ERROR: '{}' is not a file.", path.display());
        return Err(());
    }

    let exe_path = build_files(cli, path, linker_args)?;
    println!("Running executable '{}'", exe_path.display());
    let exe_name = exe_path.file_name().expect("Executable path must be valid");
    let exit_status = Command::new(&exe_path).status().map_err(|err| {
        eprintln!("ERROR: Unable to spawn executable: {err}");
    })?;
    if let Some(exit_code) = exit_status.code()
        && exit_code != 0
    {
        eprintln!(
            "Executable '{}' exited with code '{}'",
            exe_name.display(),
            exit_code
        );
    }
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = exit_status.signal() {
            eprintln!(
                "Executable '{}' exited with signal '{}'",
                exe_name.display(),
                signal
            );
        }
    }
    if !exit_status.success() {
        return Err(());
    }
    Ok(())
}

/* fn check_file(file_path: &Path, verbose: bool) -> Result<(), Vec<Spanned<CodeError>>> {
    let (program, src) = parse_path(file_path, verbose).map_err(|err| match err {
        ParseFailure::Io(p, e) => BuildError::Io(p, e),
        ParseFailure::Lex(p) => BuildError::Lexer(p),
        ParseFailure::Parse(p) => BuildError::Parser(p),
    });

    todo!();
}*/

fn build_file(
    file_path: &Path,
    output_dir: &Path,
    verbose: bool,
    release: bool,
    linker_args: &str,
    runtime_lib: &Path,
) -> Result<PathBuf, BuildError> {
    let base_name = file_path.file_name().expect("Input must be file");
    let output_file = output_dir.join(base_name);

    let src = fs::read_to_string(file_path)
        .map_err(|err| BuildError::Io(file_path.to_path_buf(), err))?;

    let program = parse_source(&src, verbose).map_err(|err| {
        let diagnostic_path = diagnostic_path(file_path);
        let src_report = Source::from(src.as_str());
        err.report(&diagnostic_path, &src_report);
        match err {
            ParseFailure::Lex(_) => BuildError::Lexer(file_path.to_path_buf()),
            ParseFailure::Parse(_) => BuildError::Parser(file_path.to_path_buf()),
        }
    })?;

    if verbose {
        println!();
    }

    let product = match generate(program, file_path, &src, verbose, release) {
        Ok(product) => product,
        Err(errors) => match errors {
            GeneratorErrors::ModuleErrors(modules) => {
                let diagnostic_path = diagnostic_path(file_path);
                for module_errors in modules {
                    let src_report = Source::from(module_errors.src.as_str());
                    report_compile_errors(&diagnostic_path, &src_report, module_errors.errors);
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
            GeneratorErrors::MissingMain => {
                eprintln!("error: program has no `fn main()` entry point");
                return Err(BuildError::Generator(file_path.into()));
            }
        },
    };

    let bytes = product
        .emit()
        .map_err(|_| BuildError::Generator(file_path.into()))?;

    let object_file = output_file.with_extension("o");
    let mut f =
        File::create(&object_file).map_err(|err| BuildError::Io(object_file.clone(), err))?;
    f.write_all(&bytes)
        .map_err(|err| BuildError::Io(object_file.clone(), err))?;

    if verbose {
        println!("Wrote object to '{}'.", object_file.display());
    }

    #[cfg(target_os = "windows")]
    let exe_file = output_file.with_extension("exe");

    #[cfg(not(target_os = "windows"))]
    let exe_file = output_file.with_extension("");

    let mut command = Command::new("gcc");
    command.arg("-o").arg(&exe_file).arg(&object_file);

    if !linker_args.is_empty() {
        command.args(linker_args.split(' ')); // TODO: handle path with spaces etc
    }

    command.arg(runtime_lib);

    if verbose {
        println!("{command:?}");
    }

    let status = command
        .status()
        .map_err(|_| BuildError::Linker(file_path.into()))?;

    if !status.success() {
        return Err(BuildError::Linker(file_path.into()));
    }

    if verbose {
        println!("Saved executable to '{}'", exe_file.display());
    }

    Ok(exe_file)
}

fn diagnostic_path(file_path: &Path) -> String {
    let path = if file_path.is_absolute() {
        env::current_dir()
            .ok()
            .and_then(|cwd| {
                let cwd = cwd.canonicalize().unwrap_or(cwd);
                let file_path = file_path
                    .canonicalize()
                    .unwrap_or_else(|_| file_path.to_path_buf());
                file_path.strip_prefix(cwd).ok().map(Path::to_path_buf)
            })
            .unwrap_or_else(|| file_path.to_path_buf())
    } else {
        file_path.to_path_buf()
    };

    path.to_string_lossy().replace('\\', "/")
}

fn clean() -> Result<(), ()> {
    println!("Cleaning build directory.");
    let output_dir = env::current_dir()
        .map_err(|err| eprintln!("{err}"))?
        .join(OUTPUT_DIR);

    if output_dir.exists() {
        remove_dir_all(&output_dir)
            .map_err(|err| eprintln!("ERROR: Unable to clean build directory: {err}"))?;
    }

    println!("Cleaned build directory '{}'.", output_dir.display());
    Ok(())
}

const RUNTIME_LIB: &[u8] = include_bytes!(env!("RUNTIME_LIB_PATH"));

fn extract_runtime_lib() -> io::Result<PathBuf> {
    let temp_dir = env::temp_dir();
    let lib_name = env!("RUNTIME_LIB_NAME");
    let lib_path = temp_dir.join(lib_name);

    let mut file = File::create(&lib_path)?;
    file.write_all(RUNTIME_LIB)?;

    Ok(lib_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_test_files() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test");
        for release in [false, true] {
            let cli = Cli {
                release,
                verbose: false,
                command: CliCommand::Test { path: path.clone() },
            };

            assert!(test_files(&cli, &path).is_ok());
        }
    }
}
