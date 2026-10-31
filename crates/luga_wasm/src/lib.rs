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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offset_counts_code_units() {
        // 'é' is 2 bytes and 1 UTF-16 unit, '😀' is 4 bytes and 2 units
        let src = "aé😀b";
        assert_eq!(utf16_offset(src, 0), 0);
        assert_eq!(utf16_offset(src, 1), 1);
        assert_eq!(utf16_offset(src, 3), 2);
        assert_eq!(utf16_offset(src, 7), 4);
        assert_eq!(utf16_offset(src, 8), 5);
        // Offsets inside a character round down, offsets past the end are clamped
        assert_eq!(utf16_offset(src, 2), 1);
        assert_eq!(utf16_offset(src, 100), 5);
    }

    #[test]
    fn check_reports_utf16_offsets_under_the_given_key() {
        let files = HashMap::from([
            (
                "main.luga",
                "import lib.greet as g;\n\nfn main() {\n    g.greet();\n}\n",
            ),
            (
                "lib/greet.luga",
                "fn greet() {\n    var s = \"héllo\";\n    var x = s + 2;\n}\n",
            ),
        ]);
        let json = check("main.luga", &serde_json::to_string(&files).unwrap()).unwrap();
        let diagnostics: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert_eq!(diagnostics.len(), 1, "{json}");
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic["path"], "lib/greet.luga");
        // `s + 2` starts at byte 47, but at UTF-16 offset 46 because of the 'é'
        assert_eq!(diagnostic["from"], 46);
        assert_eq!(diagnostic["to"], 51);
    }
}
