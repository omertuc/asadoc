//! `asadoc serve`: the review UI and the API behind it. The UI (browser JS)
//! only presents; everything it shows is computed here, and every change it
//! asks for is made here.

use crate::config::Config;
use crate::docs;
use crate::eval::{self, CandidateInfo, Evaluation, Formerly};
use crate::ignored::{self, Ignored};
use crate::lightbulb;
use crate::markers::MarkerOption;
use crate::matching::Values;
use crate::repo::{MarkedCode, Problem};
use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::path::Component;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.mjs");
const APP_CSS: &str = include_str!("../ui/app.css");
const GUIDE: &str = include_str!("../GUIDE.md");

struct AppState {
    config: Config,
    changes: broadcast::Sender<()>,
}

type Shared = Arc<AppState>;

pub fn serve(config: Config, port: u16) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let (changes, _) = broadcast::channel(16);
        let state = Arc::new(AppState { config, changes });
        let _watcher = watch(state.clone())?;
        let app = Router::new()
            .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX) }))
            .route("/app.mjs", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
            .route("/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }))
            .route("/guide", get(|| async { ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], GUIDE) }))
            .route("/api/data", get(data))
            .route("/api/module", get(module))
            .route("/api/file", get(file))
            .route("/api/events", get(events))
            .route("/api/fix", post(fix))
            .route("/api/ignore", post(ignore))
            .route("/api/unignore", post(unignore))
            .route("/api/remove-ignored", post(remove_ignored))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        println!("asadoc: review UI at http://localhost:{port}");
        axum::serve(listener, app).await?;
        anyhow::Ok(())
    })
}

// ---------------------------------------------------------------------------
// Live updates: tell open pages when either repo changes
// ---------------------------------------------------------------------------

fn watch(state: Shared) -> Result<notify::RecommendedWatcher> {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        let ignored = |p: &std::path::Path| p.components().any(|c| matches!(c.as_os_str().to_str(), Some(".git" | "node_modules" | "target")));
        if event.paths.iter().any(|p| !ignored(p)) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&state.config.repo_root, RecursiveMode::Recursive)?;
    watcher.watch(&state.config.docs_root.join("modules"), RecursiveMode::NonRecursive)?;
    // Debounce: one notification once changes settle
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            while tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_ok_and(|v| v.is_some()) {}
            let _ = state.changes.send(());
        }
    });
    Ok(watcher)
}

async fn events(State(state): State<Shared>) -> impl IntoResponse {
    let stream = BroadcastStream::new(state.changes.subscribe()).map(|_| Ok::<_, std::convert::Infallible>(Event::default().data("changed")));
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
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

fn code_ref(code: &MarkedCode, values: Option<&Values>) -> CodeRef {
    CodeRef {
        id: code.id.clone(),
        file: code.file.clone(),
        snippet: code.section.clone(),
        lines: code.lines,
        marker_lines: code.marker_lines.clone(),
        options: code.options.clone(),
        doc_options: code.doc_options.clone(),
        values: values.cloned(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockData {
    asm: String,
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
    code: Option<Vec<CodeRef>>,
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
struct Link {
    asm: String,
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
    code: CodeRef,
    matched_by: Vec<Link>,
    resembled_by: Vec<Link>,
}

#[derive(Serialize)]
struct Stale {
    reason: String,
    content: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    guides: Vec<GuideData>,
    total: usize,
    stale_ignored: Vec<Stale>,
    code: Vec<CodeData>,
    problems: Vec<Problem>,
    docs_repo: String,
    links: crate::config::Links,
}

fn report(config: &Config, ev: &Evaluation) -> Data {
    let mut guides = Vec::new();
    let mut total = 0;
    for a in &ev.assemblies {
        total += a.blocks.len();
        let base = |b: &eval::BlockEval| BlockData {
            asm: a.assembly.id.clone(),
            reference: b.block.reference.clone(),
            module: b.block.module.clone(),
            lang: b.block.lang.clone(),
            seq: b.block.seq,
            line: b.block.line,
            content: b.block.content.clone(),
            section: b.block.section.clone(),
            lead: b.block.lead.clone(),
            candidates: None,
            formerly: None,
            code: None,
            ignored_as: None,
        };
        guides.push(GuideData {
            id: a.assembly.id.clone(),
            title: a.assembly.title.clone(),
            blocks: a
                .blocks
                .iter()
                .filter(|b| !b.done())
                .map(|b| BlockData { candidates: Some(b.candidates.clone()), formerly: Some(b.formerly.clone()), ..base(b) })
                .collect(),
            resolved: a
                .blocks
                .iter()
                .filter(|b| b.resolved())
                .map(|b| BlockData {
                    code: Some(b.matches.iter().map(|m| code_ref(&ev.scan.marked[m.code], Some(&m.values))).collect()),
                    ..base(b)
                })
                .collect(),
            ignored: a
                .blocks
                .iter()
                .filter(|b| b.ignored_as.is_some())
                .map(|b| BlockData { ignored_as: b.ignored_as.clone(), ..base(b) })
                .collect(),
        });
    }
    let code = ev
        .scan
        .marked
        .iter()
        .enumerate()
        .map(|(i, c)| CodeData {
            code: code_ref(c, None),
            matched_by: ev
                .blocks()
                .filter_map(|(a, b)| {
                    let m = b.matches.iter().find(|m| m.code == i)?;
                    Some(Link { asm: a.assembly.id.clone(), reference: b.block.reference.clone(), values: Some(m.values.clone()), similarity: None })
                })
                .collect(),
            resembled_by: ev
                .blocks()
                .filter_map(|(a, b)| {
                    let cand = b.candidates.iter().find(|x| x.id == c.id)?;
                    Some(Link { asm: a.assembly.id.clone(), reference: b.block.reference.clone(), values: None, similarity: Some(cand.similarity) })
                })
                .collect(),
        })
        .collect();
    Data {
        guides,
        total,
        stale_ignored: ev.stale_ignored.iter().map(|(r, c)| Stale { reason: r.clone(), content: c.clone() }).collect(),
        code,
        problems: ev.scan.problems.clone(),
        docs_repo: config.docs_root.display().to_string(),
        links: config.links.clone(),
    }
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(serde_json::json!({ "error": message.to_string() }))).into_response()
}

async fn with_evaluation<T: Send + 'static>(
    state: &Shared,
    f: impl FnOnce(&Config, Evaluation) -> Result<T, Response> + Send + 'static,
) -> Result<T, Response> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let ev = eval::evaluate(&state.config, true).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        f(&state.config, ev)
    })
    .await
    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
}

async fn data(State(state): State<Shared>) -> Response {
    match with_evaluation(&state, |config, ev| Ok(report(config, &ev))).await {
        Ok(d) => Json(d).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct ModuleQuery {
    asm: String,
    module: String,
}

/// A module's text and the attributes it needs, for rendering in the browser
async fn module(State(state): State<Shared>, Query(q): Query<ModuleQuery>) -> Response {
    let config = &state.config;
    let Some(assembly) = config.assemblies.iter().find(|a| docs::read_assembly(&config.docs_root, a).id == q.asm) else {
        return error(StatusCode::NOT_FOUND, "no such assembly");
    };
    if !q.module.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        return error(StatusCode::BAD_REQUEST, "bad module name");
    }
    match docs::module_for_rendering(&config.docs_root, &q.module) {
        Some(text) => Json(serde_json::json!({
            "text": text,
            "attributes": docs::assembly_attributes(&config.docs_root, assembly),
        }))
        .into_response(),
        None => error(StatusCode::NOT_FOUND, "no such module"),
    }
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

/// A repo file's text; only plain relative paths inside the repo
async fn file(State(state): State<Shared>, Query(q): Query<FileQuery>) -> Response {
    let path = std::path::Path::new(&q.path);
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return error(StatusCode::BAD_REQUEST, "bad path");
    }
    match std::fs::read_to_string(state.config.repo_root.join(path)) {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "no such file"),
    }
}

// ---------------------------------------------------------------------------
// Actions (each re-evaluates first, so it acts on the current state of both repos)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BlockAction {
    asm: String,
    #[serde(rename = "ref")]
    reference: String,
    id: Option<String>,
    reason: Option<String>,
    replacing: Option<String>,
}

fn done() -> Result<Json<serde_json::Value>, Response> {
    Ok(Json(serde_json::json!({ "success": true })))
}

async fn fix(State(state): State<Shared>, Json(a): Json<BlockAction>) -> Response {
    let r = with_evaluation(&state, move |config, ev| {
        let block = ev.find(&a.asm, &a.reference).ok_or_else(|| error(StatusCode::NOT_FOUND, format!("doc block {} not found", a.reference)))?;
        let id = a.id.unwrap_or_default();
        let cand = block.candidates.iter().find(|c| c.id == id);
        let (cand, plan) = cand
            .and_then(|c| c.plan.as_ref().map(|p| (c, p)))
            .ok_or_else(|| error(StatusCode::CONFLICT, format!("{id} has no fix for {} anymore", a.reference)))?;
        lightbulb::apply(&config.repo_root, &cand.file, cand.name.as_deref(), plan).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        println!("Fixed {id} for {}", a.reference);
        done()
    })
    .await;
    r.map_or_else(|e| e, IntoResponse::into_response)
}

fn update_ignored(config: &Config, f: impl FnOnce(&mut Ignored) -> Result<()>) -> Result<(), Response> {
    let mut ig = Ignored::load(&config.ignore_dir).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    f(&mut ig).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

async fn ignore(State(state): State<Shared>, Json(a): Json<BlockAction>) -> Response {
    let r = with_evaluation(&state, move |config, ev| {
        let reason = a.reason.clone().unwrap_or_default();
        if !ignored::REASONS.contains(&reason.as_str()) {
            return Err(error(StatusCode::BAD_REQUEST, "invalid reason"));
        }
        let block = ev.find(&a.asm, &a.reference).ok_or_else(|| error(StatusCode::NOT_FOUND, format!("doc block {} not found", a.reference)))?;
        update_ignored(config, |ig| ig.ignore(&block.block.content, &reason, &block.block.reference, a.replacing.as_deref()))?;
        println!("Ignored {} as {reason}", a.reference);
        done()
    })
    .await;
    r.map_or_else(|e| e, IntoResponse::into_response)
}

async fn unignore(State(state): State<Shared>, Json(a): Json<BlockAction>) -> Response {
    let r = with_evaluation(&state, move |config, ev| {
        let block = ev.find(&a.asm, &a.reference).filter(|b| b.ignored_as.is_some());
        let block = block.ok_or_else(|| error(StatusCode::NOT_FOUND, format!("{} isn't ignored", a.reference)))?;
        update_ignored(config, |ig| ig.remove(&block.block.content))?;
        println!("Stopped ignoring {}", a.reference);
        done()
    })
    .await;
    r.map_or_else(|e| e, IntoResponse::into_response)
}

#[derive(Deserialize)]
struct RemoveIgnored {
    content: String,
}

async fn remove_ignored(State(state): State<Shared>, Json(a): Json<RemoveIgnored>) -> Response {
    let config = &state.config;
    match update_ignored(config, |ig| ig.remove(&a.content)) {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(r) => r,
    }
}
