//! `asadoc serve`: the review UI and the API behind it. The UI (browser JS)
//! only presents; everything it shows is computed here, and every change it
//! asks for is made here.

use crate::config::{AsadocConfig, Docs};
use crate::docs;
use crate::eval::{self, AssemblyEval, BlockEval, CandidateInfo, Evaluation, FormerIgnoredEntry};
use crate::ignored::{IgnoreReason, IgnoredBlocks};
use crate::lightbulb;
use crate::markers::{self, MarkerOption};
use crate::matching::PlaceholderValues;
use crate::progress;
use crate::repo::{MarkedCode, MarkerProblem, Todo};
use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.mjs");
const APP_CSS: &str = include_str!("../ui/app.css");
const GUIDE_HTML: &str = include_str!("../ui/guide.html");
const GUIDE_MARKDOWN: &str = include_str!("../GUIDE.md");

struct AppState {
    /// Set once the config is loaded; until the first evaluation is done too,
    /// the API answers 503 and the page shows `preparation`
    config: OnceLock<AsadocConfig>,
    preparation: Mutex<Preparation>,
    /// Tells open pages there's a new evaluation
    change_sender: broadcast::Sender<()>,
    /// Kept for as long as the server runs, once there's a config to watch
    watcher: Mutex<Option<RecommendedWatcher>>,
    /// Counts the changes made to the repos (seen by the watcher, or made by
    /// an action), so an evaluation knows whether it's current
    changes_seen: AtomicU64,
    /// Wakes the evaluator when `changes_seen` goes up
    evaluation_needed: Notify,
    /// The latest evaluation, kept current in the background
    latest: watch::Sender<Option<Arc<LatestEvaluation>>>,
}

type SharedState = Arc<AppState>;

enum Preparation {
    Preparing { since: Instant },
    Ready,
    Failed(String),
}

/// An evaluation of the repos as they were after `changes_seen` changes
struct LatestEvaluation {
    changes_seen: u64,
    /// The evaluation and the report built from it, or why that failed
    outcome: Result<(Evaluation, ReportData), String>,
}

impl AppState {
    fn set_preparation(&self, preparation: Preparation) {
        *self.preparation.lock().unwrap_or_else(PoisonError::into_inner) = preparation;
    }

    /// The config, once the review UI is ready; a 503 before
    fn config(&self) -> ApiResult<&AsadocConfig> {
        self.config
            .get()
            .ok_or_else(|| anyhow!("the review UI isn't ready yet"))
            .status(StatusCode::SERVICE_UNAVAILABLE)
    }

    /// Notes a change to the repos, for the evaluator to catch up with
    fn mark_changed(&self) {
        self.changes_seen.fetch_add(1, Ordering::SeqCst);
        self.evaluation_needed.notify_one();
    }

    /// The evaluation of the repos as they are now: the latest one, after
    /// waiting for the evaluator when a change came after it
    async fn current_evaluation(&self) -> ApiResult<Arc<LatestEvaluation>> {
        self.config()?;
        let mut latest = self.latest.subscribe();
        let current = latest
            .wait_for(|latest| {
                latest
                    .as_ref()
                    .is_some_and(|latest| latest.changes_seen == self.changes_seen.load(Ordering::SeqCst))
            })
            .await
            .context("waiting for the evaluation")?
            .clone()
            .context("no evaluation yet")?;
        Ok(current)
    }
}

impl LatestEvaluation {
    fn evaluated(&self) -> ApiResult<(&Evaluation, &ReportData)> {
        self.outcome
            .as_ref()
            .map(|(evaluation, report)| (evaluation, report))
            .map_err(|error| anyhow!("{error}").into())
    }
}

/// Serves right away, so a page opened early says what asadoc is busy with,
/// then loads the config and evaluates in the background, and again whenever
/// the repos change
pub(crate) fn serve(load_config: impl FnOnce() -> Result<AsadocConfig> + Send + 'static, port: u16) -> Result<()> {
    let std_listener =
        std::net::TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("listening on port {port}"))?;
    std_listener
        .set_nonblocking(true)
        .context("making the listener non-blocking")?;
    let runtime = Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async move {
        let (change_sender, _) = broadcast::channel(16);
        let state = Arc::new(AppState {
            config: OnceLock::new(),
            preparation: Mutex::new(Preparation::Preparing { since: Instant::now() }),
            change_sender,
            watcher: Mutex::new(None),
            changes_seen: AtomicU64::new(0),
            evaluation_needed: Notify::new(),
            latest: watch::Sender::new(None),
        });
        let listener = TcpListener::from_std(std_listener).context("setting up the listener")?;
        println!("asadoc: review UI at http://localhost:{port} (preparing it...)");
        tokio::spawn(prepare(Arc::clone(&state), load_config));
        axum::serve(listener, router(state)).await.context("serving HTTP")?;
        anyhow::Ok(())
    })
}

/// Loads the config, warms up, starts watching the repos and evaluates them,
/// then keeps the evaluation current. A failure to get that far is shown on
/// the page (and in the terminal) until asadoc is restarted.
async fn prepare(state: SharedState, load_config: impl FnOnce() -> Result<AsadocConfig> + Send + 'static) {
    let preparing_state = Arc::clone(&state);
    let prepared = task::spawn_blocking(move || {
        let state = preparing_state;
        progress::step("loading the config");
        let config = load_config()?;
        // Watching before evaluating, so a change made meanwhile gets evaluated too
        let watcher = watch_repos(&config, Arc::clone(&state)).context("watching the repos for changes")?;
        *state.watcher.lock().unwrap_or_else(PoisonError::into_inner) = Some(watcher);
        warm_up(&config).context("preparing the review UI")?;
        state
            .config
            .set(config)
            .map_err(|_already_set| anyhow!("the review UI was prepared twice"))?;
        let first_evaluation = evaluate_now(&state)?;
        if let Err(error) = &first_evaluation.outcome {
            return Err(anyhow!("{error}"));
        }
        state.latest.send_replace(Some(first_evaluation));
        anyhow::Ok(())
    })
    .await
    .context("preparing the review UI")
    .and_then(|prepared| prepared);
    match prepared {
        Ok(()) => {
            println!("asadoc: the review UI is ready");
            state.set_preparation(Preparation::Ready);
            keep_evaluating(state).await;
        }
        Err(error) => {
            let message = format!("{error:#}");
            eprintln!("asadoc: {message}");
            state.set_preparation(Preparation::Failed(message));
        }
    }
}

/// Evaluates the repos as they are now (blocking)
fn evaluate_now(state: &AppState) -> Result<Arc<LatestEvaluation>> {
    let config = state.config.get().context("evaluating before the config is loaded")?;
    let changes_seen = state.changes_seen.load(Ordering::SeqCst);
    let outcome = eval::evaluate(config, true)
        .context("evaluating the doc blocks")
        .and_then(|evaluation| {
            progress::step("building the report");
            let report = build_report(config, &evaluation).context("building the report")?;
            Ok((evaluation, report))
        })
        .map_err(|error| format!("{error:#}"));
    Ok(Arc::new(LatestEvaluation { changes_seen, outcome }))
}

/// Re-evaluates whenever the repos change, then tells open pages
async fn keep_evaluating(state: SharedState) {
    loop {
        let is_current = state
            .latest
            .borrow()
            .as_ref()
            .is_some_and(|latest| latest.changes_seen == state.changes_seen.load(Ordering::SeqCst));
        if is_current {
            state.evaluation_needed.notified().await;
            continue;
        }
        let evaluating_state = Arc::clone(&state);
        match task::spawn_blocking(move || evaluate_now(&evaluating_state)).await {
            Ok(Ok(latest)) => {
                if let Err(error) = &latest.outcome {
                    eprintln!("asadoc: {error}");
                }
                state.latest.send_replace(Some(latest));
                // Fails only when no page is listening
                state.change_sender.send(()).ok();
            }
            Ok(Err(error)) => eprintln!("asadoc: {error:#}"),
            Err(error) => eprintln!("asadoc: evaluating: {error}"),
        }
    }
}

/// Fetches up front what pages need besides the evaluation: the attributes
/// modules render with. With docs from git, that fetches the files into the
/// cache, so a page opens without waiting on it
fn warm_up(config: &AsadocConfig) -> Result<()> {
    for docs in &config.docs {
        for assembly_path in &docs.assemblies {
            progress::step(format!("reading the attributes of {}", docs.qualify(assembly_path)));
            docs::assembly_attributes(&docs.source, assembly_path)
                .with_context(|| format!("reading the attributes of {}", docs.qualify(assembly_path)))?;
        }
    }
    Ok(())
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
            get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], GUIDE_HTML) }),
        )
        .route(
            "/guide.md",
            get(|| async { ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], GUIDE_MARKDOWN) }),
        )
        .route("/api/status", get(serve_status))
        .route("/api/data", get(serve_data))
        .route("/api/module", get(serve_module))
        .route("/api/file", get(serve_file))
        .route("/api/events", get(serve_events))
        .route("/api/fix", post(apply_fix))
        .route("/api/remove-markers", post(remove_code_markers))
        .route("/api/ignore", post(ignore_block))
        .route("/api/unignore", post(unignore_block))
        .route("/api/remove-ignored", post(remove_ignored_content))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Live updates: tell open pages when either repo changes
// ---------------------------------------------------------------------------

fn watch_repos(config: &AsadocConfig, state: SharedState) -> Result<RecommendedWatcher> {
    let (raw_changes_sender, raw_changes) = mpsc::unbounded_channel::<()>();
    let mut watcher = recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                eprintln!("asadoc: watching for changes: {error}");
                return;
            }
        };
        // Reads (asadoc's own, while evaluating, included) change nothing
        let is_read = matches!(event.kind, EventKind::Access(access) if access != AccessKind::Close(AccessMode::Write));
        if !is_read && event.paths.iter().any(|path| !is_generated(path)) {
            // Fails only once the debouncer is gone, as the server shuts down
            raw_changes_sender.send(()).ok();
        }
    })
    .context("creating the file watcher")?;
    // Code and docs from git are fixed at the fetched commit
    for code_root in config
        .code
        .iter()
        .filter_map(|code_source| code_source.tree.local_root())
    {
        watcher
            .watch(code_root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", code_root.display()))?;
    }
    for modules_dir in config.docs.iter().filter_map(|docs| docs.source.local_modules_dir()) {
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
        // Open pages hear of it once it's evaluated
        state.mark_changed();
    }
}

/// Whether the review UI is ready; while it's being prepared, what asadoc is
/// busy with and for how long
async fn serve_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    Json(
        match &*state.preparation.lock().unwrap_or_else(PoisonError::into_inner) {
            Preparation::Preparing { since } => json!({
                "state": "preparing",
                "step": progress::current_step(),
                "seconds": since.elapsed().as_secs(),
            }),
            Preparation::Ready => json!({ "state": "ready" }),
            Preparation::Failed(error) => json!({ "state": "failed", "error": error }),
        },
    )
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
    #[serde(rename = "lines")]
    line_range: Option<(usize, usize)>,
    marker_lines: Vec<usize>,
    options: Vec<MarkerOption>,
    doc_options: Vec<MarkerOption>,
    values: Option<PlaceholderValues>,
    /// Where its file is on the web, when its code source says
    link: Option<String>,
}

fn code_ref(config: &AsadocConfig, marked_code: &MarkedCode, values: Option<&PlaceholderValues>) -> CodeRef {
    let link = config
        .code_file(&marked_code.file)
        .ok()
        .and_then(|(code_source, path)| {
            code_source
                .links
                .as_ref()
                .map(|links| links.file(path, marked_code.line_range))
        });
    CodeRef {
        link,
        id: marked_code.id.clone(),
        file: marked_code.file.clone(),
        snippet: marked_code.section.clone(),
        line_range: marked_code.line_range,
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
    #[serde(rename = "lang")]
    language: String,
    #[serde(rename = "seq")]
    position_in_language: usize,
    line: usize,
    content: String,
    section: Option<String>,
    lead: Option<String>,
    /// Where the block is on the web, when its docs source says
    link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidates: Option<Vec<CandidateInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "formerly")]
    former_ignored_entries: Option<Vec<FormerIgnoredEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "code")]
    matched_code: Option<Vec<CodeRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored_as: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuideData {
    id: String,
    title: String,
    /// The name of its docs source
    docs_name: String,
    /// Where its docs are, for people
    docs_location: String,
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
    values: Option<PlaceholderValues>,
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
    ignore_reasons: Vec<IgnoreReason>,
    #[serde(rename = "code")]
    marked_code: Vec<CodeData>,
    problems: Vec<MarkerProblem>,
    todos: Vec<Todo>,
}

/// Each marked code, with the blocks that match or resemble it
fn code_data(config: &AsadocConfig, evaluation: &Evaluation) -> Vec<CodeData> {
    evaluation
        .scan
        .marked
        .iter()
        .enumerate()
        .map(|(code_index, marked_code)| CodeData {
            code_ref: code_ref(config, marked_code, None),
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
            let code_match = block
                .matches
                .iter()
                .find(|code_match| code_match.marked_index == code_index)?;
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
            let docs = config
                .docs
                .get(assembly.assembly.docs_index)
                .with_context(|| format!("no docs source for {}", assembly.assembly.id))?;
            guide_data(config, evaluation, docs, assembly)
                .with_context(|| format!("reporting on {}", assembly.assembly.id))
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
        ignore_reasons: evaluation.ignore_reasons.clone(),
        marked_code: code_data(config, evaluation),
        problems: evaluation.scan.problems.clone(),
        todos: evaluation.scan.todos.clone(),
    })
}

/// An assembly's blocks: to resolve, resolved, and ignored
fn guide_data(
    config: &AsadocConfig,
    evaluation: &Evaluation,
    docs: &Docs,
    assembly: &AssemblyEval,
) -> Result<GuideData> {
    Ok(GuideData {
        id: assembly.assembly.id.clone(),
        title: assembly.assembly.title.clone(),
        docs_name: docs.name.clone(),
        docs_location: docs.source.describe(),
        blocks: assembly
            .blocks
            .iter()
            .filter(|block| !block.done())
            .map(|block| BlockData {
                candidates: Some(block.candidates.clone()),
                former_ignored_entries: Some(block.former_ignored_entries.clone()),
                ..block_data(docs, assembly, block)
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
                    .map(|code_match| {
                        Ok(code_ref(
                            config,
                            evaluation.marked(code_match.marked_index)?,
                            Some(&code_match.values),
                        ))
                    })
                    .collect::<Result<_>>()
                    .with_context(|| format!("listing the code {} matches", block.block.reference))?;
                Ok(BlockData {
                    matched_code: Some(matched_code),
                    ..block_data(docs, assembly, block)
                })
            })
            .collect::<Result<_>>()?,
        ignored: assembly
            .blocks
            .iter()
            .filter(|block| block.ignored_as.is_some())
            .map(|block| BlockData {
                ignored_as: block.ignored_as.clone(),
                ..block_data(docs, assembly, block)
            })
            .collect(),
    })
}

/// What every listing shows of a block
fn block_data(docs: &Docs, assembly: &AssemblyEval, block: &BlockEval) -> BlockData {
    BlockData {
        link: docs
            .links
            .as_ref()
            .map(|links| links.source_line(&docs::module_path(&block.block.module), block.block.line)),
        assembly_id: assembly.assembly.id.clone(),
        reference: block.block.reference.clone(),
        module: block.block.module.clone(),
        language: block.block.language.clone(),
        position_in_language: block.block.position_in_language,
        line: block.block.line,
        content: block.block.content.clone(),
        section: block.block.section.clone(),
        lead: block.block.lead.clone(),
        candidates: None,
        former_ignored_entries: None,
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

/// Runs an action on the current evaluation; it changes the repos, so the
/// evaluation that follows waits for re-evaluating
async fn with_evaluation<T: Send + 'static>(
    state: &SharedState,
    act: impl FnOnce(&AsadocConfig, &Evaluation) -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    let latest = state.current_evaluation().await?;
    let acting_state = Arc::clone(state);
    let outcome = task::spawn_blocking(move || {
        let (evaluation, _) = latest.evaluated()?;
        act(acting_state.config()?, evaluation)
    })
    .await
    .context("running the action")?;
    state.mark_changed();
    outcome
}

async fn serve_data(State(state): State<SharedState>) -> ApiResult {
    let latest = state.current_evaluation().await?;
    let (_, report) = latest.evaluated()?;
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
    let (docs, assembly_path) = find_assembly(state.config()?, &module_query.assembly_id)
        .context("finding the assembly")?
        .with_context(|| format!("no assembly {}", module_query.assembly_id))
        .status(StatusCode::NOT_FOUND)?;
    if !is_plain_module_name(&module_query.module) {
        return Err(anyhow!("bad module name {:?}", module_query.module)).status(StatusCode::BAD_REQUEST);
    }
    let module_text = docs::module_for_rendering(&docs.source, &module_query.module)
        .with_context(|| format!("reading the module {}", module_query.module))?
        .with_context(|| format!("no module {}", module_query.module))
        .status(StatusCode::NOT_FOUND)?;
    let attributes = docs::assembly_attributes(&docs.source, assembly_path)
        .with_context(|| format!("reading the attributes of {assembly_path}"))?;
    Ok(Json(json!({ "text": module_text, "attributes": attributes })).into_response())
}

/// The docs source and path of the assembly with the given `assembly_id`, if any
fn find_assembly<'config>(
    config: &'config AsadocConfig,
    assembly_id: &str,
) -> Result<Option<(&'config Docs, &'config String)>> {
    for (docs_index, docs) in config.docs.iter().enumerate() {
        for assembly_path in &docs.assemblies {
            let assembly = docs::read_assembly(docs, docs_index, assembly_path)
                .with_context(|| format!("reading the assembly {}", docs.qualify(assembly_path)))?;
            if assembly.id == assembly_id {
                return Ok(Some((docs, assembly_path)));
            }
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

/// A code file's text, by its name among all the code sources; only plain
/// relative paths inside a source
async fn serve_file(State(state): State<SharedState>, Query(file_query): Query<FileQuery>) -> ApiResult {
    let (code_source, path) = state.config()?.code_file(&file_query.path)?;
    let is_plain_relative = Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !is_plain_relative {
        return Err(anyhow!("bad path {:?}", file_query.path)).status(StatusCode::BAD_REQUEST);
    }
    let file_text = code_source
        .tree
        .read(path)
        .with_context(|| format!("reading {}", file_query.path))?
        .with_context(|| format!("no text file {}", file_query.path))
        .status(StatusCode::NOT_FOUND)?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], file_text).into_response())
}

// ---------------------------------------------------------------------------
// Actions (each on the current evaluation, so it acts on the current state of both repos)
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
    /// With `reason`, when ignoring: a new reason, meaning this
    #[serde(rename = "newReason")]
    new_reason_description: Option<String>,
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
        let block = find_block(evaluation, &action)?;
        let candidate_id = action.candidate_id.unwrap_or_default();
        let (candidate, fix_plan) = block
            .candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .and_then(|candidate| candidate.plan.as_ref().map(|fix_plan| (candidate, fix_plan)))
            .with_context(|| format!("{candidate_id} has no fix for {} anymore", action.reference))
            .status(StatusCode::CONFLICT)?;
        let file_path = config
            .writable_code_file(&candidate.file)
            .status(StatusCode::CONFLICT)?;
        lightbulb::apply(&file_path, candidate.section_name.as_deref(), fix_plan)
            .with_context(|| format!("applying the fix to {candidate_id}"))?;
        println!("Fixed {candidate_id} for {}", action.reference);
        Ok(success_response())
    })
    .await
}

#[derive(Deserialize)]
struct CodeAction {
    /// The marked code's id (`file` or `file#section`)
    id: String,
}

/// Removes the markers of a piece of marked code, leaving its code as it is
async fn remove_code_markers(State(state): State<SharedState>, Json(action): Json<CodeAction>) -> ApiResult {
    with_evaluation(&state, move |config, evaluation| {
        let marked_code = evaluation
            .scan
            .marked
            .iter()
            .find(|marked_code| marked_code.id == action.id)
            .with_context(|| format!("{} isn't marked anymore", action.id))
            .status(StatusCode::CONFLICT)?;
        let file_path = config
            .writable_code_file(&marked_code.file)
            .status(StatusCode::CONFLICT)?;
        let text = std::fs::read_to_string(&file_path).with_context(|| format!("reading {}", file_path.display()))?;
        let unmarked = markers::remove_markers(&text, marked_code.section.as_deref())
            .with_context(|| format!("removing the markers of {}", action.id))
            .status(StatusCode::CONFLICT)?;
        std::fs::write(&file_path, unmarked).with_context(|| format!("writing {}", file_path.display()))?;
        println!("Removed the markers of {}", action.id);
        Ok(success_response())
    })
    .await
}

/// Loads the ignore directory and makes a change to it
fn update_ignored(config: &AsadocConfig, change: impl FnOnce(&mut IgnoredBlocks) -> Result<()>) -> Result<()> {
    let mut ignored_blocks = IgnoredBlocks::load_all(config).context("loading the ignore directories")?;
    change(&mut ignored_blocks)
}

async fn ignore_block(State(state): State<SharedState>, Json(action): Json<BlockAction>) -> ApiResult {
    with_evaluation(&state, move |config, evaluation| {
        let reason = action.reason.clone().unwrap_or_default();
        let is_known_reason = evaluation
            .ignore_reasons
            .iter()
            .any(|known_reason| known_reason.name == reason);
        if is_known_reason == action.new_reason_description.is_some() {
            let problem = if is_known_reason {
                "already exists"
            } else {
                "doesn't exist"
            };
            return Err(anyhow!("the reason {reason:?} {problem}")).status(StatusCode::BAD_REQUEST);
        }
        let block = find_block(evaluation, &action)?;
        update_ignored(config, |ignored_blocks| {
            if let Some(description) = &action.new_reason_description {
                ignored_blocks
                    .add_reason(&reason, description)
                    .with_context(|| format!("adding the reason {reason}"))?;
            }
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
    update_ignored(state.config()?, |ignored_blocks| {
        ignored_blocks.unignore(&request.content)
    })
    .context("removing ignored content")?;
    state.mark_changed();
    Ok(success_response())
}
