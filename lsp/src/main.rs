use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use luga::CompileError;
use luga_syntax::{SimpleSpan, Spanned};
use tracing_subscriber::EnvFilter;

use crate::text::Text;

mod text;

#[derive(Debug)]
struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Url, Document>>>,
    diagnostic_sets: Arc<Mutex<HashMap<Url, HashSet<Url>>>>,
}

#[derive(Debug, Clone)]
struct Document {
    text: Text,
    version: i32,
}

pub const LANGUAGE_ID: &str = "luga";

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("initialize");
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "luga_lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("initialized");
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("shutdown");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        tracing::info!("Opening document: {}", &params.text_document.uri);
        if params.text_document.language_id != LANGUAGE_ID {
            tracing::debug!(
                "Ignoring unsupported document language_id={} uri={}",
                params.text_document.language_id,
                params.text_document.uri
            );
            return;
        }

        let document = Document {
            version: params.text_document.version,
            text: params.text_document.text.into(),
        };
        let existing = self
            .documents
            .lock()
            .await
            .insert(params.text_document.uri.clone(), document);

        if existing.is_some() {
            tracing::warn!("Opened existing document {}", params.text_document.uri);
        }

        self.launch_check(params.text_document.uri, params.text_document.version);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut documents = self.documents.lock().await;
        let Some(doc) = documents.get_mut(&params.text_document.uri) else {
            tracing::warn!(
                "Received didChange for unknown document. {}",
                params.text_document.uri
            );
            return;
        };

        for change in params.content_changes {
            match change.range {
                Some(range) => doc.text.update(range.start, range.end, &change.text),
                None => doc.text.update_full(&change.text),
            }
        }

        doc.version = params.text_document.version;
        self.launch_check(params.text_document.uri, doc.version);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let mut documents = self.documents.lock().await;
        let Some(doc) = documents.get_mut(&params.text_document.uri) else {
            tracing::warn!(
                "Received didSave for unknown document. {}",
                params.text_document.uri
            );
            return;
        };

        if let Some(text) = params.text {
            doc.text.update_full(&text);
        }

        self.launch_check(params.text_document.uri, doc.version);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let root_uri = params.text_document.uri;
        let diagnostic_root_uri = root_uri
            .to_file_path()
            .ok()
            .as_ref()
            .and_then(|path| path_to_uri(path))
            .unwrap_or_else(|| root_uri.clone());

        let open_document_uris = {
            let mut documents = self.documents.lock().await;
            documents.remove(&root_uri);
            documents.keys().cloned().collect::<HashSet<_>>()
        };

        let stale_diagnostic_uris = self
            .diagnostic_sets
            .lock()
            .await
            .remove(&diagnostic_root_uri)
            .unwrap_or_default();

        for uri in stale_diagnostic_uris {
            if open_document_uris.contains(&uri) {
                continue;
            }
            self.client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }
}

fn path_to_uri(path: &Path) -> Option<Url> {
    let path = path.canonicalize().ok()?;
    Url::from_file_path(path).ok()
}

impl Backend {
    fn launch_check(&self, uri: Url, version: i32) {
        let client = self.client.clone();
        let documents = self.documents.clone();
        let diagnostic_sets = self.diagnostic_sets.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;

            let documents_guard = documents.lock().await;
            let Some(doc) = documents_guard.get(&uri) else {
                return;
            };

            if doc.version != version {
                return; // stale check
            }

            tracing::info!("Running check on {}", &uri);
            let path = uri.to_file_path().ok();
            let diagnostic_root_uri = path
                .as_ref()
                .and_then(|path| path_to_uri(path))
                .unwrap_or_else(|| uri.clone());

            let source = doc.text.to_string();
            drop(documents_guard);

            let Some(modules) = luga::check_source(path, source.as_str(), false) else {
                tracing::warn!("Unable to check file {}", &uri);
                return;
            };

            let still_current = documents
                .lock()
                .await
                .get(&uri)
                .is_some_and(|doc| doc.version == version);

            if !still_current {
                return;
            }

            let mut current_diagnostic_uris = HashSet::new();

            for module in modules {
                let Some(path) = module.path else {
                    continue;
                };

                let Some(module_uri) = path_to_uri(&path) else {
                    continue;
                };

                let publish_uri = if module_uri == diagnostic_root_uri {
                    uri.clone()
                } else {
                    module_uri.clone()
                };

                current_diagnostic_uris.insert(publish_uri.clone());

                let diagnostics: Vec<Diagnostic> = module
                    .errors
                    .iter()
                    .filter(|e| !matches!(e, CompileError::ModuleInner))
                    .map(|err| {
                        let Spanned(message, span) = error_spanned_message(err);
                        Diagnostic {
                            range: span_to_range(&module.src, span),
                            severity: Some(DiagnosticSeverity::ERROR),
                            message,
                            ..Default::default()
                        }
                    })
                    .collect();

                tracing::debug!(
                    uri = %publish_uri,
                    diagnostics = diagnostics.len(),
                    "Publishing diagnostics"
                );

                let diagnostic_version = if publish_uri == uri {
                    Some(version)
                } else {
                    None
                };

                client
                    .publish_diagnostics(publish_uri, diagnostics, diagnostic_version)
                    .await;
            }

            let stale_diagnostic_uris = {
                let mut diagnostic_sets = diagnostic_sets.lock().await;
                let previous_diagnostic_uris = diagnostic_sets
                    .entry(diagnostic_root_uri.clone())
                    .or_default();
                let stale_diagnostic_uris = previous_diagnostic_uris
                    .difference(&current_diagnostic_uris)
                    .cloned()
                    .collect::<Vec<_>>();

                *previous_diagnostic_uris = current_diagnostic_uris;
                stale_diagnostic_uris
            };

            for old_uri in stale_diagnostic_uris {
                let diagnostic_version = if old_uri == diagnostic_root_uri {
                    Some(version)
                } else {
                    None
                };

                client
                    .publish_diagnostics(old_uri, Vec::new(), diagnostic_version)
                    .await;
            }
        });
    }
}

#[tokio::main]
async fn main() {
    let log_file =
        File::create(std::env::temp_dir().join("luga-lsp.log")).expect("failed to create log file");

    tracing_subscriber::fmt()
        .with_writer(log_file)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(tracing::Level::DEBUG.into())
                .add_directive("cranelift_codegen=off".parse().unwrap())
                .add_directive("cranelift_object=off".parse().unwrap())
                .add_directive("cranelift_frontend=off".parse().unwrap())
                .add_directive("cranelift_module=off".parse().unwrap()),
        )
        .init();

    tracing::info!("starting luga LSP server");
    tracing::info!("Hello!");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::new(Mutex::new(HashMap::new())),
        diagnostic_sets: Arc::new(Mutex::new(HashMap::new())),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn error_spanned_message(err: &CompileError) -> Spanned<String> {
    match err {
        CompileError::Code(Spanned(err, span)) => Spanned(err.to_string(), *span),
        CompileError::Lex(Spanned(err, span)) => Spanned(err.to_string(), *span),
        CompileError::Parse(Spanned(err, span)) => Spanned(err.to_string(), *span),
        CompileError::ModuleInner => unreachable!("not reported as a diagnostic"),
    }
}

fn offset_to_position(src: &str, offset: usize) -> Position {
    let mut line = 0;
    let mut line_start = 0;

    for (idx, ch) in src.char_indices() {
        if idx >= offset {
            break;
        }

        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }

    let character = src[line_start..offset].encode_utf16().count() as u32;

    Position { line, character }
}

fn span_to_range(src: &str, span: SimpleSpan) -> Range {
    Range {
        start: offset_to_position(src, span.start),
        end: offset_to_position(src, span.end),
    }
}
