//! `asadoc serve`: the review UI and the API behind it. The UI (browser JS)
//! only presents; everything it shows is computed here, and every change it
//! asks for is made here.

use crate::config::{AsadocConfig, Links};
use crate::docs;
use crate::eval::{self, AssemblyEval, BlockEval, CandidateInfo, Evaluation, Formerly};
use crate::ignored::{self, Ignored};
use crate::lightbulb;
use crate::markers::MarkerOption;
use crate::matching::Values;
use crate::repo::{self, MarkedCode, Problem};
use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc};
use tokio::task;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.mjs");
const APP_CSS: &str = include_str!("../ui/app.css");
const GUIDE_MARKDOWN: &str = include_str!("../GUIDE.md");

struct AppState {
    config: AsadocConfig,
    change_sender: broadcast::Sender<()>,
}

type SharedState = Arc<AppState>;

pub(crate) fn serve(config: AsadocConfig, port: u16) -> Result<()> {
    let runtime = Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async move {
        let (change_sender, _) = broadcast::channel(16);
        let state = Arc::new(AppState { config, change_sender });
        let _watcher = watch_repos(Arc::clone(&state)).context("watching the repos for changes")?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("listening on port {port}"))?;
        println!("asadoc: review UI at http://localhost:{port}");
        axum::serve(listener, router(state)).await.context("serving HTTP")?;
        anyhow::Ok(())
    })
}

fn router(state: SharedState) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML) }),
        )
        .route(
            "/app.mjs",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }),
        )
        .route(
            "/app.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }),
        )
        .route(
            "/guide",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], GUIDE_MARKDOWN) }),
        )
        .route("/api/data", get(serve_data))
        .route("/api/module", get(serve_module))
        .route("/api/file", get(serve_file))
        .route("/api/events", get(serve_events))
        .route("/api/fix", post(apply_fix))
        .route("/api/ignore", post(ignore_block))
        .route("/api/unignore", post(unignore_block))
        .route("/api/remove-ignored", post(remove_ignored_content))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Live updates: tell open pages when either repo changes
// ---------------------------------------------------------------------------

fn watch_repos(state: SharedState) -> Result<RecommendedWatcher> {
    let (raw_changes_sender, raw_changes) = mpsc::unbounded_channel::<()>();
    let mut watcher = recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                eprintln!("asadoc: watching for changes: {error}");
                return;
            }
        };
        if event.paths.iter().any(|path| !is_generated(path)) {
            // Fails only once the debouncer is gone, as the server shuts down
            raw_changes_sender.send(()).ok();
        }
    })
    .context("creating the file watcher")?;
    let repo_root = &state.config.repo_root;
    watcher
        .watch(repo_root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", repo_root.display()))?;
    // Docs from git are fixed at the fetched commit
    if let Some(modules_dir) = state.config.docs.local_modules_dir() {
        watcher
            .watch(&modules_dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching {}", modules_dir.display()))?;
    }
    tokio::spawn(debounce_changes(raw_changes, state));
    Ok(watcher)
}

/// Whether a change to `path` is one open pages don't care about
fn is_generated(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component.as_os_str().to_str(), Some(".git" | "node_modules" | "target")))
}

/// One notification to open pages once changes settle
async fn debounce_changes(mut raw_changes: mpsc::UnboundedReceiver<()>, state: SharedState) {
    while raw_changes.recv().await.is_some() {
        while timeout(Duration::from_millis(300), raw_changes.recv())
            .await
            .is_ok_and(|received| received.is_some())
        {}
        // Fails only when no page is listening
        state.change_sender.send(()).ok();
    }
}

async fn serve_events(State(state): State<SharedState>) -> impl IntoResponse {
    let change_events = BroadcastStream::new(state.change_sender.subscribe())
        .map(|_| Ok::<_, Infallible>(Event::default().data("changed")));
    Sse::new(change_events).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeRef {
    id: String,
    file: String,
    snippet: Option<String>,
    lines: Option<(usize, usize)>,
    marker_lines: Vec<usize>,
    options: Vec<MarkerOption>,
    doc_options: Vec<MarkerOption>,
    values: Option<Values>,
}

fn code_ref(marked_code: &MarkedCode, values: Option<&Values>) -> CodeRef {
    CodeRef {
        id: marked_code.id.clone(),
        file: marked_code.file.clone(),
        snippet: marked_code.section.clone(),
        lines: marked_code.lines,
        marker_lines: marked_code.marker_lines.clone(),
        options: marked_code.options.clone(),
        doc_options: marked_code.doc_options.clone(),
        values: values.cloned(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockData {
    #[serde(rename = "asm")]
    assembly_id: String,
    #[serde(rename = "ref")]
    reference: String,
    module: String,
    lang: String,
    seq: usize,
    line: usize,
    content: String,
    section: Option<String>,
    lead: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidates: Option<Vec<CandidateInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    formerly: Option<Vec<Formerly>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "code")]
    matched_code: Option<Vec<CodeRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored_as: Option<String>,
}

#[derive(Serialize)]
struct GuideData {
    id: String,
    title: String,
    blocks: Vec<BlockData>,
    resolved: Vec<BlockData>,
    ignored: Vec<BlockData>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockLink {
    #[serde(rename = "asm")]
    assembly_id: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Values>,
    #[serde(skip_serializing_if = "Option::is_none")]
    similarity: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CodeData {
    #[serde(flatten)]
    code_ref: CodeRef,
    matched_by: Vec<BlockLink>,
    resembled_by: Vec<BlockLink>,
}

#[derive(Serialize)]
struct StaleIgnored {
    reason: String,
    content: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportData {
    guides: Vec<GuideData>,
    #[serde(rename = "total")]
    total_blocks: usize,
    stale_ignored: Vec<StaleIgnored>,
    #[serde(rename = "code")]
    marked_code: Vec<CodeData>,
    problems: Vec<Problem>,
    /// Where the docs are, for people
    docs_location: String,
    links: Links,
}

/// Each marked code, with the blocks that match or resemble it
fn code_data(evaluation: &Evaluation) -> Vec<CodeData> {
    evaluation
        .scan
        .marked
        .iter()
        .enumerate()
        .map(|(code_index, marked_code)| CodeData {
            code_ref: code_ref(marked_code, None),
            matched_by: matched_by(evaluation, code_index),
            resembled_by: resembled_by(evaluation, marked_code),
        })
        .collect()
}

/// The blocks that match the marked code at `code_index`
fn matched_by(evaluation: &Evaluation, code_index: usize) -> Vec<BlockLink> {
    evaluation
        .blocks()
        .filter_map(|(assembly, block)| {
            let code_match = block.matches.iter().find(|code_match| code_match.code == code_index)?;
            Some(BlockLink {
                assembly_id: assembly.assembly.id.clone(),
                reference: block.block.reference.clone(),
                values: Some(code_match.values.clone()),
                similarity: None,
            })
        })
        .collect()
}

/// The blocks that `marked_code` is a candidate for
fn resembled_by(evaluation: &Evaluation, marked_code: &MarkedCode) -> Vec<BlockLink> {
    evaluation
        .blocks()
        .filter_map(|(assembly, block)| {
            let candidate = block
                .candidates
                .iter()
                .find(|candidate| candidate.id == marked_code.id)?;
            Some(BlockLink {
                assembly_id: assembly.assembly.id.clone(),
                reference: block.block.reference.clone(),
                values: None,
                similarity: Some(candidate.similarity),
            })
        })
        .collect()
}

fn build_report(config: &AsadocConfig, evaluation: &Evaluation) -> Result<ReportData> {
    let guides = evaluation
        .assemblies
        .iter()
        .map(|assembly| {
            guide_data(evaluation, assembly).with_context(|| format!("reporting on {}", assembly.assembly.id))
        })
        .collect::<Result<_>>()?;
    Ok(ReportData {
        guides,
        total_blocks: evaluation.assemblies.iter().map(|assembly| assembly.blocks.len()).sum(),
        stale_ignored: evaluation
            .stale_ignored
            .iter()
            .map(|(reason, content)| StaleIgnored {
                reason: reason.clone(),
                content: content.clone(),
            })
            .collect(),
        marked_code: code_data(evaluation),
        problems: evaluation.scan.problems.clone(),
        docs_location: config.docs.describe(),
        links: config.links.clone(),
    })
}

/// An assembly's blocks: to resolve, resolved, and ignored
fn guide_data(evaluation: &Evaluation, assembly: &AssemblyEval) -> Result<GuideData> {
    Ok(GuideData {
        id: assembly.assembly.id.clone(),
        title: assembly.assembly.title.clone(),
        blocks: assembly
            .blocks
            .iter()
            .filter(|block| !block.done())
            .map(|block| BlockData {
                candidates: Some(block.candidates.clone()),
                formerly: Some(block.formerly.clone()),
                ..block_data(assembly, block)
            })
            .collect(),
        resolved: assembly
            .blocks
            .iter()
            .filter(|block| block.resolved())
            .map(|block| {
                let matched_code = block
                    .matches
                    .iter()
                    .map(|code_match| Ok(code_ref(evaluation.marked(code_match.code)?, Some(&code_match.values))))
                    .collect::<Result<_>>()
                    .with_context(|| format!("listing the code {} matches", block.block.reference))?;
                Ok(BlockData {
                    matched_code: Some(matched_code),
                    ..block_data(assembly, block)
                })
            })
            .collect::<Result<_>>()?,
        ignored: assembly
            .blocks
            .iter()
            .filter(|block| block.ignored_as.is_some())
            .map(|block| BlockData {
                ignored_as: block.ignored_as.clone(),
                ..block_data(assembly, block)
            })
            .collect(),
    })
}

/// What every listing shows of a block
fn block_data(assembly: &AssemblyEval, block: &BlockEval) -> BlockData {
    BlockData {
        assembly_id: assembly.assembly.id.clone(),
        reference: block.block.reference.clone(),
        module: block.block.module.clone(),
        lang: block.block.lang.clone(),
        seq: block.block.seq,
        line: block.block.line,
        content: block.block.content.clone(),
        section: block.block.section.clone(),
        lead: block.block.lead.clone(),
        candidates: None,
        formerly: None,
        matched_code: None,
        ignored_as: None,
    }
}

/// A failed API request: the status to answer with, and why
struct ApiError {
    status: StatusCode,
    error: anyhow::Error,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = format!("{:#}", self.error);
        if self.status.is_server_error() {
            eprintln!("asadoc: {message}");
        }
        (self.status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error,
        }
    }
}

/// Answers an error with a status other than 500
trait WithStatus<T> {
    fn status(self, status: StatusCode) -> Result<T, ApiError>;
}

impl<T> WithStatus<T> for Result<T> {
    fn status(self, status: StatusCode) -> Result<T, ApiError> {
        self.map_err(|error| ApiError { status, error })
    }
}

type ApiResult<T = Response> = Result<T, ApiError>;

async fn with_evaluation<T: Send + 'static>(
    state: &SharedState,
    respond: impl FnOnce(&AsadocConfig, Evaluation) -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    let state = Arc::clone(state);
    task::spawn_blocking(move || {
        let evaluation = eval::evaluate(&state.config, true).context("evaluating the doc blocks")?;
        respond(&state.config, evaluation)
    })
    .await
    .context("running the evaluation")?
}

async fn serve_data(State(state): State<SharedState>) -> ApiResult {
    let report = with_evaluation(&state, |config, evaluation| {
        Ok(build_report(config, &evaluation).context("building the report")?)
    })
    .await?;
    Ok(Json(report).into_response())
}

#[derive(Deserialize)]
struct ModuleQuery {
    #[serde(rename = "asm")]
    assembly_id: String,
    module: String,
}

/// A module's text and the attributes it needs, for rendering in the browser
async fn serve_module(State(state): State<SharedState>, Query(module_query): Query<ModuleQuery>) -> ApiResult {
    let config = &state.config;
    let assembly_path = find_assembly(config, &module_query.assembly_id)
        .context("finding the assembly")?
        .with_context(|| format!("no assembly {}", module_query.assembly_id))
        .status(StatusCode::NOT_FOUND)?;
    if !is_plain_module_name(&module_query.module) {
        return Err(anyhow!("bad module name {:?}", module_query.module)).status(StatusCode::BAD_REQUEST);
    }
    let module_text = docs::module_for_rendering(&config.docs, &module_query.module)
        .with_context(|| format!("reading the module {}", module_query.module))?
        .with_context(|| format!("no module {}", module_query.module))
        .status(StatusCode::NOT_FOUND)?;
    let attributes = docs::assembly_attributes(&config.docs, assembly_path)
        .with_context(|| format!("reading the attributes of {assembly_path}"))?;
    Ok(Json(json!({ "text": module_text, "attributes": attributes })).into_response())
}

/// The path of the assembly with the given `assembly_id`, if any
fn find_assembly<'config>(config: &'config AsadocConfig, assembly_id: &str) -> Result<Option<&'config String>> {
    for assembly_path in &config.assemblies {
        let assembly = docs::read_assembly(&config.docs, assembly_path)
            .with_context(|| format!("reading the assembly {assembly_path}"))?;
        if assembly.id == assembly_id {
            return Ok(Some(assembly_path));
        }
    }
    Ok(None)
}

fn is_plain_module_name(module_name: &str) -> bool {
    module_name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

/// A repo file's text; only plain relative paths inside the repo
async fn serve_file(State(state): State<SharedState>, Query(file_query): Query<FileQuery>) -> ApiResult {
    let is_plain_relative = Path::new(&file_query.path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !is_plain_relative {
        return Err(anyhow!("bad path {:?}", file_query.path)).status(StatusCode::BAD_REQUEST);
    }
    let file_text = repo::read(&state.config.repo_root, &file_query.path)
        .with_context(|| format!("reading {}", file_query.path))?
        .with_context(|| format!("no text file {}", file_query.path))
        .status(StatusCode::NOT_FOUND)?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], file_text).into_response())
}

// ---------------------------------------------------------------------------
// Actions (each re-evaluates first, so it acts on the current state of both repos)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BlockAction {
    #[serde(rename = "asm")]
    assembly_id: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(rename = "id")]
    candidate_id: Option<String>,
    reason: Option<String>,
    #[serde(rename = "replacing")]
    replacing_content: Option<String>,
}

fn success_response() -> Response {
    Json(json!({ "success": true })).into_response()
}

/// The doc block an action is on
fn find_block<'evaluation>(
    evaluation: &'evaluation Evaluation,
    action: &BlockAction,
) -> ApiResult<&'evaluation BlockEval> {
    evaluation
        .find(&action.assembly_id, &action.reference)
        .with_context(|| format!("doc block {} not found", action.reference))
        .status(StatusCode::NOT_FOUND)
}

async fn apply_fix(State(state): State<SharedState>, Json(action): Json<BlockAction>) -> ApiResult {
    with_evaluation(&state, move |config, evaluation| {
        let block = find_block(&evaluation, &action)?;
        let candidate_id = action.candidate_id.unwrap_or_default();
        let (candidate, fix_plan) = block
            .candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .and_then(|candidate| candidate.plan.as_ref().map(|fix_plan| (candidate, fix_plan)))
            .with_context(|| format!("{candidate_id} has no fix for {} anymore", action.reference))
            .status(StatusCode::CONFLICT)?;
        lightbulb::apply(&config.repo_root, &candidate.file, candidate.name.as_deref(), fix_plan)
            .with_context(|| format!("applying the fix to {candidate_id}"))?;
        println!("Fixed {candidate_id} for {}", action.reference);
        Ok(success_response())
    })
    .await
}

/// Loads the ignore directory and makes a change to it
fn update_ignored(config: &AsadocConfig, change: impl FnOnce(&mut Ignored) -> Result<()>) -> Result<()> {
    let mut ignored_blocks = Ignored::load(&config.ignore_dir).context("loading the ignore directory")?;
    change(&mut ignored_blocks)
}

async fn ignore_block(State(state): State<SharedState>, Json(action): Json<BlockAction>) -> ApiResult {
    with_evaluation(&state, move |config, evaluation| {
        let reason = action.reason.clone().unwrap_or_default();
        if !ignored::IGNORE_REASONS.contains(&reason.as_str()) {
            return Err(anyhow!("invalid reason {reason:?}")).status(StatusCode::BAD_REQUEST);
        }
        let block = find_block(&evaluation, &action)?;
        update_ignored(config, |ignored_blocks| {
            ignored_blocks.ignore(
                &block.block.content,
                &reason,
                &block.block.reference,
                action.replacing_content.as_deref(),
            )
        })
        .with_context(|| format!("ignoring {}", action.reference))?;
        println!("Ignored {} as {reason}", action.reference);
        Ok(success_response())
    })
    .await
}

async fn unignore_block(State(state): State<SharedState>, Json(action): Json<BlockAction>) -> ApiResult {
    with_evaluation(&state, move |config, evaluation| {
        let block = evaluation
            .find(&action.assembly_id, &action.reference)
            .filter(|block| block.ignored_as.is_some())
            .with_context(|| format!("{} isn't ignored", action.reference))
            .status(StatusCode::NOT_FOUND)?;
        update_ignored(config, |ignored_blocks| ignored_blocks.unignore(&block.block.content))
            .with_context(|| format!("un-ignoring {}", action.reference))?;
        println!("Stopped ignoring {}", action.reference);
        Ok(success_response())
    })
    .await
}

#[derive(Deserialize)]
struct RemoveIgnoredRequest {
    content: String,
}

async fn remove_ignored_content(
    State(state): State<SharedState>,
    Json(request): Json<RemoveIgnoredRequest>,
) -> ApiResult {
    update_ignored(&state.config, |ignored_blocks| {
        ignored_blocks.unignore(&request.content)
    })
    .context("removing ignored content")?;
    Ok(success_response())
}
