use crate::config::Config;
use log::debug;
use std::collections::HashMap;
use std::default::Default;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use sv_parser::{parse_sv_str, Define, DefineText, SyntaxTree};
use svlint::config::Config as LintConfig;
use svlint::linter::Linter;
use tokio::sync::mpsc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

#[derive(Debug)]
enum Job {
    Init { root_uri: Option<Url> },
    DocumentUpdated(Url),
}

pub struct Backend {
    client: Client,
    worker_queue: mpsc::UnboundedSender<Job>,
    worker_handler: tokio::task::JoinHandle<()>,
    semantic_db: Arc<RwLock<sv_language_index::Db>>,
}

pub struct BackendWorker {
    client: Client,
    root_uri: Option<Url>,
    config: Config,
    linter: Option<RwLock<Linter>>,
    semantic_db: Arc<RwLock<sv_language_index::Db>>,
}

trait IntoLsp {
    type Target: Sized;

    fn into_lsp(&self) -> Self::Target;
}

trait FromLsp {
    type Source: Sized;

    fn from_lsp(source: &Self::Source) -> Self;
}

impl IntoLsp for sv_language_index::position::Position {
    type Target = tower_lsp::lsp_types::Position;

    fn into_lsp(&self) -> Self::Target {
        Self::Target {
            line: self.row,
            character: self.col,
        }
    }
}

impl IntoLsp for sv_language_index::position::Range {
    type Target = tower_lsp::lsp_types::Range;

    fn into_lsp(&self) -> Self::Target {
        Self::Target {
            start: self.begin.into_lsp(),
            end: self.end.into_lsp(),
        }
    }
}

impl FromLsp for sv_language_index::position::Position {
    type Source = tower_lsp::lsp_types::Position;

    fn from_lsp(source: &Self::Source) -> Self {
        Self {
            row: source.line,
            col: source.character,
        }
    }
}

impl BackendWorker {
    fn process(&self, path: PathBuf, document: &str) -> Vec<Diagnostic> {
        let mut ret = Vec::new();

        let root_uri = if let Some(ref root_uri) = self.root_uri {
            if let Ok(root_uri) = root_uri.to_file_path() {
                root_uri
            } else {
                PathBuf::from("")
            }
        } else {
            PathBuf::from("")
        };

        let mut include_paths = Vec::new();
        let mut defines = HashMap::new();
        for path in &self.config.verilog.include_paths {
            let mut p = root_uri.clone();
            p.push(PathBuf::from(path));
            include_paths.push(p);
        }
        for define in &self.config.verilog.defines {
            let mut define = define.splitn(2, '=');
            let ident = String::from(define.next().unwrap());
            let text = define
                .next()
                .and_then(|x| enquote::unescape(x, None).ok())
                .map(|x| DefineText::new(x, None));
            let define = Define::new(ident.clone(), vec![], text);
            defines.insert(ident, Some(define));
        }
        debug!("include_paths: {:?}", include_paths);
        debug!("defines: {:?}", defines);

        let parsed = parse_sv_str(
            document,
            &PathBuf::from(""),
            &defines,
            &include_paths,
            false,
            false,
        );
        match parsed {
            Ok((syntax_tree, _new_defines)) => {
                let file_id = self.semantic_db.write().unwrap().update(path, &syntax_tree);
                ret = self.lint(file_id, &syntax_tree);
            }
            Err(x) => {
                debug!("parse_error: {:?}", x);
                if let sv_parser::Error::Parse(Some((path, pos))) = x {
                    if path.as_path() == Path::new("") {
                        let (line, col) = get_position(document, pos);
                        let line_end = get_line_end(document, pos);
                        let len = line_end - pos as u32;
                        ret.push(Diagnostic::new(
                            Range::new(Position::new(line, col), Position::new(line, col + len)),
                            Some(DiagnosticSeverity::ERROR),
                            None,
                            Some(String::from("svls")),
                            String::from("parse error"),
                            None,
                            None,
                        ));
                    }
                }
            }
        }

        ret
    }

    fn lint(
        &self,
        file_id: sv_language_index::FileId,
        syntax_tree: &SyntaxTree,
    ) -> Vec<Diagnostic> {
        let db = self.semantic_db.read().unwrap();
        let data = db.get_data(file_id).unwrap();
        let mut ret = Vec::new();
        if let Some(linter) = &self.linter {
            let mut linter = linter.write().unwrap();
            for event in syntax_tree.into_iter().event() {
                for failed in linter.check(&syntax_tree, &event) {
                    debug!("{:?}", failed);
                    if failed.path != PathBuf::from("") {
                        continue;
                    }
                    let begin_position = data.line_index.offset_to_position(failed.beg).into_lsp();
                    ret.push(Diagnostic::new(
                        Range::new(
                            begin_position.clone(),
                            Position::new(
                                begin_position.line,
                                begin_position.character + failed.len as u32,
                            ),
                        ),
                        Some(DiagnosticSeverity::WARNING),
                        Some(NumberOrString::String(failed.name)),
                        Some(String::from("svls")),
                        failed.hint,
                        None,
                        None,
                    ));
                }
            }
        }

        ret
    }
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let (worker_queue, mut receiver) = mpsc::unbounded_channel();
        let semantic_db: Arc<RwLock<sv_language_index::Db>> = Default::default();

        let worker_handler = tokio::spawn({
            let client = client.clone();
            let semantic_db = semantic_db.clone();
            let worker_queue = worker_queue.clone();
            async move {
                let first_job = receiver.recv().await;

                let root_uri = if let Some(Job::Init { root_uri }) = first_job {
                    root_uri
                } else {
                    client
                        .show_message(MessageType::ERROR, "First job must be initialization")
                        .await;
                    return;
                };
                eprintln!("root_uri: {:?}", root_uri);

                let config_svls = search_config(&PathBuf::from(".svls.toml"));
                eprintln!("config_svls: {:?}", config_svls);
                let config = match generate_config(config_svls) {
                    Ok(x) => x,
                    Err(x) => {
                        client.show_message(MessageType::WARNING, &x).await;
                        Config::default()
                    }
                };

                let linter = if config.option.linter {
                    let config_svlint = search_config_svlint(&PathBuf::from(".svlint.toml"));
                    eprintln!("config_svlint: {:?}", config_svlint);

                    let linter = match generate_linter(config_svlint) {
                        Ok(x) => x,
                        Err(x) => {
                            client.show_message(MessageType::WARNING, &x).await;
                            Linter::new(LintConfig::new().enable_all())
                        }
                    };

                    Some(linter)
                } else {
                    None
                };

                let worker = BackendWorker {
                    client,
                    root_uri,
                    config,
                    linter: linter.map(RwLock::new),
                    semantic_db,
                };

                if let Some(root_uri) = &worker.root_uri {
                    let mut dirs = vec![root_uri.to_file_path().unwrap()];
                    while let Some(dir) = dirs.pop() {
                        let mut inner_dir = tokio::fs::read_dir(&dir).await.unwrap();
                        while let Ok(Some(entry)) = inner_dir.next_entry().await {
                            let file_type = entry.file_type().await.unwrap();
                            if file_type.is_dir() {
                                dirs.push(entry.path());
                            } else if file_type.is_file() {
                                if let Some(ext) =
                                    entry.path().extension().and_then(std::ffi::OsStr::to_str)
                                {
                                    match ext {
                                        "v" | "sv" => worker_queue
                                            .send(Job::DocumentUpdated(
                                                Url::from_file_path(entry.path()).unwrap(),
                                            ))
                                            .unwrap(),
                                        _ => { /* ignore */ }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    eprintln!("root uri is not set");
                }

                while let Some(job) = receiver.recv().await {
                    match job {
                        Job::Init { .. } => unreachable!("Already initialized"),
                        Job::DocumentUpdated(url) => match url.to_file_path() {
                            Ok(path) => {
                                eprintln!("process {:?}", path);
                                let document = tokio::fs::read_to_string(&path).await.unwrap();
                                let diags = worker.process(path, &document);
                                worker.client.publish_diagnostics(url, diags, None).await;
                            }
                            Err(_) => worker.client.show_message(MessageType::ERROR, "").await,
                        },
                    }
                }
            }
        });

        Self {
            client,
            worker_queue,
            worker_handler,
            semantic_db,
        }
    }
}

#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.worker_queue
            .send(Job::Init {
                root_uri: params.root_uri,
            })
            .map_err(|_| {
                tower_lsp::jsonrpc::Error::new(tower_lsp::jsonrpc::ErrorCode::InternalError)
            })?;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: None,
                        change: None,
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                definition_provider: Some(OneOf::Left(true)),

                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: String::from("svls"),
                version: Some(String::from(env!("CARGO_PKG_VERSION"))),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        self.worker_handler.abort();
        Ok(())
    }

    async fn did_change_workspace_folders(&self, _: DidChangeWorkspaceFoldersParams) {}

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("did_open");
        if let Err(_) = self
            .worker_queue
            .send(Job::DocumentUpdated(params.text_document.uri))
        {
            self.client.show_message(MessageType::ERROR, "").await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Err(_) = self
            .worker_queue
            .send(Job::DocumentUpdated(params.text_document.uri))
        {
            self.client.show_message(MessageType::ERROR, "").await;
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<request::GotoDeclarationResponse>> {
        let file_path = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        let request_location = sv_language_index::position::DocumentPosition {
            document: file_path,
            position: FromLsp::from_lsp(&params.text_document_position_params.position),
        };
        let response = self
            .semantic_db
            .read()
            .unwrap()
            .goto_definition(request_location);
        let response = response.map(|response| {
            request::GotoDeclarationResponse::Scalar(Location {
                uri: Url::from_file_path(response.document).unwrap(),
                range: response.range.into_lsp(),
            })
        });

        Ok(response)
    }
}

fn search_config(config: &Path) -> Option<PathBuf> {
    let curr = env::current_dir().ok()?;
    curr.ancestors().find_map(|dir| {
        let candidate = dir.join(config);
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    })
}

fn search_config_svlint(config: &Path) -> Option<PathBuf> {
    if let Ok(c) = env::var("SVLINT_CONFIG") {
        let candidate = Path::new(&c);
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        } else {
            debug!(
                "SVLINT_CONFIG=\"{}\" does not exist. Searching hierarchically.",
                c
            );
        }
    }

    search_config(config)
}

fn generate_config(config: Option<PathBuf>) -> std::result::Result<Config, String> {
    let path = match config {
        Some(c) => c,
        _ => return Ok(Default::default()),
    };
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "Failed to read {}. Enable all lint rules.",
            path.to_string_lossy()
        )
    })?;
    toml::from_str(&text).map_err(|_| {
        format!(
            "Failed to parse {}. Enable all lint rules.",
            path.to_string_lossy()
        )
    })
}

fn generate_linter(config: Option<PathBuf>) -> std::result::Result<Linter, String> {
    let path =
        config.ok_or_else(|| String::from(".svlint.toml is not found. Enable all lint rules."))?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "Failed to read {}. Enable all lint rules.",
            path.to_string_lossy()
        )
    })?;
    let parsed = toml::from_str(&text).map_err(|_| {
        format!(
            "Failed to parse {}. Enable all lint rules.",
            path.to_string_lossy()
        )
    })?;
    Ok(Linter::new(parsed))
}

fn get_position(s: &str, pos: usize) -> (u32, u32) {
    let mut line = 0;
    let mut col = 0;
    let mut p = 0;
    while p < pos {
        if let Some(c) = s.get(p..p + 1) {
            if c == "\n" {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        } else {
            col += 1;
        }
        p += 1;
    }
    (line, col)
}

fn get_line_end(s: &str, pos: usize) -> u32 {
    let mut p = pos;
    while p < s.len() {
        if let Some(c) = s.get(p..p + 1) {
            if c == "\n" {
                break;
            }
        }
        p += 1;
    }
    p as u32
}
