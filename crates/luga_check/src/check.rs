use std::{
    env, fs,
    path::{Path, PathBuf},
};

use ariadne::Source;
use luga_syntax::parse_source;

use crate::loader::{FsLoader, SourceLoader};
use crate::lower_state::lower_ast;
use crate::{CompileError, LowerErrors, ModuleErrors, report_compile_errors};

#[derive(Debug)]
pub enum CheckFileError {
    Io,
    Compile,
}

pub fn check_file(file_path: &Path, verbose: bool) -> Result<(), CheckFileError> {
    let src = fs::read_to_string(file_path).map_err(|err| {
        eprintln!("Error reading file '{}': {}", file_path.display(), err);
        CheckFileError::Io
    })?;

    let Some(modules) = check_source(Some(file_path.to_path_buf()), &src, &FsLoader, verbose)
    else {
        eprintln!("Error checking {}", file_path.display());
        return Err(CheckFileError::Compile);
    };

    let errored = !modules.is_empty();

    for module_errors in modules {
        let diagnostic_path = module_errors.path.as_deref().map(diagnostic_path);
        let src_report = Source::from(module_errors.src.as_str());
        report_compile_errors(
            diagnostic_path.as_deref(),
            &src_report,
            module_errors.errors,
        );
    }

    if errored {
        Err(CheckFileError::Compile)
    } else {
        Ok(())
    }
}

pub fn check_source(
    file_path: Option<PathBuf>,
    src: &str,
    loader: &dyn SourceLoader,
    verbose: bool,
) -> Option<Vec<ModuleErrors>> {
    let ast = match parse_source(&src, verbose) {
        Ok(program) => program,
        Err(failure) => {
            let module_error = ModuleErrors {
                path: file_path.to_owned(),
                src: src.to_owned(),
                errors: CompileError::from_parse_failure(failure),
            };
            return Some(vec![module_error]);
        }
    };

    if verbose {
        println!();
    }

    match lower_ast(ast, file_path.as_deref(), &src, loader, verbose) {
        Ok(_) => return Some(Vec::new()),
        Err(errors) => match errors {
            LowerErrors::ModuleErrors(modules) => Some(modules),
            LowerErrors::MissingMain => Some(Vec::new()),
        },
    }
}

/// Return a path that is relative to the current working directory
pub fn diagnostic_path(file_path: &Path) -> String {
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
