use std::collections::HashMap;
use std::path::Path;

use luga_check::loader::normalize;
use luga_check::{CompileError, MemoryLoader, SourceLoader, check_source};
use luga_syntax::Spanned;
use serde::Serialize;
use wasm_bindgen::prelude::*;

// Offsets are UTF-16 indices into the source of `path`, as used by CodeMirror
#[derive(Serialize)]
struct LintDiagnostic {
    path: String,
    from: usize,
    to: usize,
    message: String,
    severity: &'static str,
}

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

/// Check `main` and the modules it imports. `files_json` is a JSON object mapping
/// paths to sources. Returns the diagnostics of all modules as a JSON array, with
/// each `path` being the key it was passed under.
#[wasm_bindgen]
pub fn check(main: &str, files_json: &str) -> Result<String, JsError> {
    let files: HashMap<String, String> = serde_json::from_str(files_json)?;

    let mut loader = MemoryLoader::new();
    let mut keys = HashMap::new();
    for (key, src) in files {
        keys.insert(normalize(Path::new(&key)), key.clone());
        loader.insert(&key, src);
    }

    let main_path = normalize(Path::new(main));
    let src = loader
        .load(&main_path)
        .map_err(|_| JsError::new(&format!("file not found: {main}")))?;

    let modules = check_source(Some(main_path.clone()), &src, &loader, false).unwrap_or_default();

    let diagnostics: Vec<LintDiagnostic> = modules
        .iter()
        .filter_map(|module| {
            let path = module.path.as_ref()?;
            let key = keys.get(path).cloned().unwrap_or_else(|| display(path));
            Some((module, key))
        })
        .flat_map(|(module, key)| {
            module
                .errors
                .iter()
                .filter_map(CompileError::spanned_message)
                .map(move |Spanned(message, span)| {
                    let from = utf16_offset(&module.src, span.start);
                    let to = utf16_offset(&module.src, span.end).max(from);
                    LintDiagnostic {
                        path: key.clone(),
                        from,
                        to,
                        message,
                        severity: "error",
                    }
                })
        })
        .collect();

    Ok(serde_json::to_string(&diagnostics)?)
}

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Convert a byte offset into a UTF-16 offset, clamped to the source.
fn utf16_offset(src: &str, offset: usize) -> usize {
    let mut offset = offset.min(src.len());
    while !src.is_char_boundary(offset) {
        offset -= 1;
    }
    src[..offset].encode_utf16().count()
}
