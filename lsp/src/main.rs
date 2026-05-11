use std::collections::HashMap;
use std::fs::File;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::text::Text;

mod text;

#[derive(Debug)]
struct Backend {
    client: Client,
    documents: HashMap<TextDocumentIdentifier, Text>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("initialize");
        Ok(InitializeResult::default())
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

    async fn did_open(&self, _params: DidOpenTextDocumentParams) {}
}

#[tokio::main]
async fn main() {
    let log_file =
        File::create(std::env::temp_dir().join("luga-lsp.log")).expect("failed to create log file");

    tracing_subscriber::fmt()
        .with_writer(log_file)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::DEBUG.into()),
        )
        .init();

    tracing::info!("starting luga LSP server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: HashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
