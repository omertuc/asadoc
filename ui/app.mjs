// The UI only presents what the asadoc server computes, and asks it for changes.
import { FileDiff, File, parseDiffFromFile } from 'https://esm.sh/@pierre/diffs@1.4.3';
import Asciidoctor from 'https://esm.sh/@asciidoctor/core@3.0.4';

const asciidoctor = Asciidoctor();

// A queue of doc blocks that no marked repo code matches yet. Each one gets
// resolved by making some code match it (by hand, by an AI assistant, or with
// a lightbulb fix), or ignored when no repo code should match it. Resolved and
// ignored blocks, and all marked code, can be browsed too.

let data = null;
let selectedKey = null;   // `${asm}:${ref}`
let tab = 'todo';         // 'todo' | 'resolved' | 'ignored' | 'code' | 'todos'
// The tabs, under their category's title
const CATEGORIES = [
  { label: 'Doc code blocks', tabs: ['todo', 'resolved', 'ignored'] },
  { label: 'Code', tabs: ['code'] },
  { label: 'Comments', tabs: ['todos'] },
];
let query = '';
let selectedCandidate = -1;   // index of the open recommendation, -1 when all are collapsed
let ignoreChoice = null;  // the reason picked to ignore the block as (NEW_REASON for a new one), and the new one's fields
const NEW_REASON = '';
let rendered = [];        // pierre components to clean up

const root = document.getElementById('root');
root.innerHTML = `
  <aside id="queue">
    <div class="queue-head"></div>
    <div class="queue-controls">
      <div class="tabs"></div>
      <input type="search" placeholder="Search refs, files, sections, code…">
    </div>
    <div class="queue-list"></div>
  </aside>
  <main id="block"></main>
`;
const queueEl = document.getElementById('queue');
const headEl = queueEl.querySelector('.queue-head');
const listEl = queueEl.querySelector('.queue-list');
const searchEl = queueEl.querySelector('input');
const blockEl = document.getElementById('block');

queueEl.querySelector('.tabs').addEventListener('click', (e) => {
  const t = e.target.closest('[data-tab]')?.dataset.tab;
  if (!t || t === tab) return;
  tab = t;
  const first = visibleBlocks()[0];
  select(first ? keyOf(first) : null);
});
searchEl.addEventListener('input', () => { query = searchEl.value.trim().toLowerCase(); renderQueue(); });

// --- Helpers ---

function el(tag, cls, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html != null) e.innerHTML = html;
  return e;
}

function esc(s) {
  return String(s ?? '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// Blocks still to resolve
function allBlocks() {
  return data.guides.flatMap(g => g.blocks);
}

function resolvedBlocks() {
  return data.guides.flatMap(g => g.resolved);
}

function ignoredBlocks() {
  return data.guides.flatMap(g => g.ignored);
}

function tabBlocks() {
  return { todo: allBlocks, resolved: resolvedBlocks, ignored: ignoredBlocks, code: () => data.code, todos: todoCode }[tab]();
}

// The marked code with TODOs, in the order of their first TODO
function todoCode() {
  const ids = [...new Set(data.todos.map(t => t.codeId))];
  return ids.map(id => data.code.find(c => c.id === id)).filter(Boolean);
}

// Doc blocks are keyed by guide and ref, marked code by its id
function keyOf(b) {
  return b.key || `${b.asm}:${b.ref}`;
}

const isMarkedCode = b => !!b.key;

function codeName(c) {
  return c.snippet ? `${c.file} › § ${c.snippet}` : `${c.file} (whole file)`;
}

// A reason's directory name, as words: example-output → Example output
function reasonLabel(reason) {
  const words = String(reason).replace(/-/g, ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

// A name typed for a new reason, as a directory name: Example output → example-output
function reasonName(typed) {
  return typed.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
}

// How a resolved block is resolved, in a few words
function howResolved(b) {
  if (b.ignoredAs) return reasonLabel(b.ignoredAs);
  return codeName(b.code[0]) + (b.code.length > 1 ? ` (+${b.code.length - 1} more)` : '');
}

function matchesQuery(b) {
  if (!query) return true;
  const hay = [b.ref, b.content, b.section, b.ignoredAs, b.file, b.snippet, ...(Array.isArray(b.code) ? b.code : []).map(codeName),
    ...(b.candidates || []).map(c => `${c.file} ${c.name || ''}`)].join('\n').toLowerCase();
  return hay.includes(query);
}

// The current tab's blocks that match the search
function visibleBlocks() {
  return tabBlocks().filter(matchesQuery);
}

function selectedBlock() {
  return [...allBlocks(), ...resolvedBlocks(), ...ignoredBlocks(), ...data.code].find(b => keyOf(b) === selectedKey) || null;
}

// Opens a doc block, in whichever tab it is
function goToBlock(asm, ref) {
  const key = `${asm}:${ref}`;
  tab = allBlocks().some(b => keyOf(b) === key) ? 'todo' : resolvedBlocks().some(b => keyOf(b) === key) ? 'resolved' : 'ignored';
  select(key);
}

// Opens a piece of marked code in the marked code tab
function goToCode(id) {
  tab = 'code';
  select(`code:${id}`);
}

function isResolved(b) {
  return !isMarkedCode(b) && 'code' in b;
}

function preview(content) {
  const line = content.split('\n').find(l => l.trim() && l.trim() !== '---') || '';
  return line.trim().replace(/^\$\s*/, '');
}

function toast(message, kind) {
  const t = el('div', `toast ${kind}`);
  t.textContent = message;
  document.body.appendChild(t);
  setTimeout(() => t.remove(), 3500);
}

function cleanup() {
  for (const c of rendered) c.cleanUp();
  rendered = [];
  hideTip();
}

const THEME = { dark: 'github-dark', light: 'github-light' };

function renderCode(mount, name, contents) {
  try {
    const f = new File({ theme: THEME, themeType: 'system', overflow: 'wrap', disableFileHeader: true });
    f.render({ file: { name, contents }, containerWrapper: mount });
    rendered.push(f);
  } catch {
    mount.appendChild(el('pre', 'fallback', esc(contents)));
  }
}

// Marker lines stand out in the file view, their parts colored by what they are
const MARKER_CSS = `
  [data-line].dac-marker, [data-line].dac-marker [data-column-content], [data-line].dac-marker * {
    color: var(--dac-marker-fg) !important;
  }
  [data-line].dac-marker { background: var(--dac-marker-bg) !important; font-weight: 600; }
  [data-line].dac-marker .dac-comment { opacity: 0.55; font-weight: 400; }
  [data-line].dac-marker .dac-at { color: var(--dac-at-fg) !important; }
  [data-line].dac-marker .dac-kind { color: var(--dac-kind-fg) !important; }
  [data-line].dac-marker mark.dac-name {
    background: var(--dac-name-bg); color: var(--dac-name-fg) !important;
    border-radius: 3px; padding: 0 3px;
  }
  [data-line].dac-marker .dac-pipe { color: var(--dac-pipe-fg) !important; font-weight: 400; }
  [data-line].dac-marker .dac-side {
    background: var(--dac-side-bg); color: var(--dac-side-fg) !important;
    border-radius: 3px; padding: 0 3px; font-size: 0.92em;
  }
  [data-line].dac-marker .dac-key { color: var(--dac-key-fg) !important; }
  [data-line].dac-marker .dac-value { color: var(--dac-value-fg) !important; font-weight: 500; }
  [data-line].dac-marker .dac-wild { color: var(--dac-wild-fg) !important; font-weight: 700; }
  [data-line].dac-marker .dac-punct { color: var(--dac-pipe-fg) !important; font-weight: 400; }
  [data-line].dac-marker .dac-note .dac-key { font-style: italic; }
  [data-line].dac-marker .dac-note .dac-value { font-style: italic; font-weight: 400; color: var(--dac-note-fg) !important; }
  [data-line].dac-marker .dac-todo .dac-key { color: var(--dac-todo-fg) !important; font-style: normal; }
  [data-line].dac-marker .dac-unknown .dac-key { text-decoration: wavy underline var(--dac-todo-fg); }
  [data-line].dac-marker [data-tip] { cursor: help; border-radius: 3px; transition: background 0.1s; }
  [data-line].dac-marker [data-tip]:hover { background: var(--dac-hover-bg); }
`;

// What each marker option does to `target` (the code, or the doc blocks it's
// compared with), in words. `v` is its value as written, unquoted.
const OPTION_HELP = {
  'remove-prefix': (v, target) => `Strips ${tipCode(v)} from the start of the first line of ${target}, after its indentation.`,
  'remove-suffix': (v, target) => `Strips ${tipCode(v)} from the end of the last line of ${target}.`,
  'strip-line-prefix': (v, target) => `Strips ${tipCode(v)} from the start of every line of ${target} that has it.`,
  'remove-lines-starting-with': (v, target) => `Drops the lines of ${target} whose text, after indentation, starts with ${tipCode(v)}.${wildcardHelp(v)}`,
  'remove-text': (v, target, side) => `Removes every match of the regex ${tipCode(v)} from ${target}, across lines too. When it matches nothing, ${side === 'doc' ? "the doc block doesn't match" : "it's a marker problem"}, so a changed text shows up.`,
  'remove-blank-lines': (_, target) => `Drops the lines of ${target} that are empty or only whitespace.`,
  'unindent-common': (_, target) => `Removes the indentation all non-blank lines of ${target} share.`,
  reindent: (v, target) => {
    const [from, to] = v.split(/\s*->\s*/);
    return `Turns each ${from} spaces of leading indentation in ${target} into ${to} spaces.`;
  },
  param: (v, target) => /\*/.test(v)
    ? `Makes every ${tipCode(v)} in ${target} a placeholder: the other side can have any value there.${wildcardHelp(v)}`
    : `Makes ${tipCode(v)} in ${target} a placeholder: the other side can have any value in its place.`,
  comment: () => 'A note for people reading the marker. It changes nothing.',
  TODO: () => 'Work left on this marked code. It changes nothing, but <code>asadoc check</code> and the review UI list it.',
};

function tipCode(v) {
  return `<code>${esc(v)}</code>`;
}

function wildcardHelp(v) {
  const parts = [];
  if (v.includes('**')) parts.push('<code>**</code> stands for anything up to the next text on the line');
  if (/(^|[^*])\*([^*]|$)/.test(v)) parts.push('<code>*</code> for any text without whitespace');
  return parts.length ? ` Here ${parts.join(', and ')}.` : '';
}

// The parts of a marker line, as HTML: each part that means something gets a
// `data-tip`, an index into `tips`, whose entry says what it means
function markerLineHtml(text, tips) {
  const tip = (title, side, body) => tips.push({ title, side, body }) - 1;
  const out = [];
  let rest;
  const head = text.match(/^(.*?)(@docs-as-code:)(\s*)(.*)$/);
  if (head) {
    out.push(`<span class="dac-comment">${esc(head[1])}</span>`);
    out.push(`<span class="dac-at" data-tip="${tip('@docs-as-code:', null, 'Marks repo code that appears in the docs. Every doc code block is compared with what it marks, after its options are applied.')}">${esc(head[2])}</span>`, esc(head[3]));
    rest = head[4];
    const kind = rest.match(/^(file|start section|end section)(\s*)("(?:[^"\\]|\\.)*")?/);
    if (kind) {
      const what = {
        file: ['Whole file', 'Marks the whole file, apart from this marker and its option lines. A file marked whole can\'t also have sections.'],
        'start section': ['Section start', 'Marks the lines from here to the end marker with the same name, markers excluded.'],
        'end section': ['Section end', 'Ends the section started with the same name.'],
      }[kind[1]];
      out.push(`<span class="dac-kind" data-tip="${tip(what[0], null, what[1])}">${esc(kind[1])}</span>`, esc(kind[2]));
      if (kind[3]) out.push(`<mark class="dac-name" data-tip="${tip('Section name', null, 'Pairs the start marker with its end marker, and names this code in the review UI and on the command line (<code>file#name</code>).')}">${esc(kind[3])}</mark>`);
      rest = rest.slice(kind[0].length);
    }
  } else {
    const i = text.indexOf('|');
    if (i === -1) return null;
    out.push(`<span class="dac-comment">${esc(text.slice(0, i))}</span>`);
    rest = text.slice(i);
  }
  // Options: `| [doc ]key[: value]`, value quoted or `<from> -> <to>`
  const option = /(\s*)\|(\s*)(?:(doc)(\s+))?([A-Za-z][\w-]*)(?:(\s*:\s*)("(?:[^"\\]|\\.)*"|\d+\s*->\s*\d+))?/y;
  let m;
  while (rest && (m = option.exec(rest))) {
    const [, before, after, side, sideSpace, key, colon, raw] = m;
    const value = raw?.startsWith('"') ? (key === 'remove-text' ? raw.slice(1, -1).replace(/\\"/g, '"') : raw.slice(1, -1).replace(/\\(.)/g, '$1')) : raw ?? '';
    const help = OPTION_HELP[key];
    const target = side ? 'each doc block it\'s compared with' : 'the marked code';
    const body = help ? help(value, target, side ? 'doc' : 'code') : 'Not an option asadoc knows: it\'s reported as a marker problem.';
    const cls = !help ? 'dac-unknown' : key === 'TODO' ? 'dac-note dac-todo' : key === 'comment' ? 'dac-note' : '';
    let valueHtml = '';
    if (raw) {
      const shown = (key === 'param' || key === 'remove-lines-starting-with')
        ? esc(raw).replace(/\*\*|\*/g, w => `<span class="dac-wild">${w}</span>`)
        : esc(raw);
      valueHtml = `<span class="dac-punct">${esc(colon)}</span><span class="dac-value">${shown}</span>`;
    }
    out.push(esc(before), `<span class="dac-opt ${cls}" data-tip="${tip(key, help && !(key === 'comment' || key === 'TODO') ? (side ? 'doc' : 'code') : null, body)}">`
      + `<span class="dac-pipe">|</span>${esc(after)}`
      + (side ? `<span class="dac-side">${side}</span>${esc(sideSpace)}` : '')
      + `<span class="dac-key">${esc(key)}</span>${valueHtml}</span>`);
    rest = rest.slice(option.lastIndex);
    option.lastIndex = 0;
  }
  out.push(esc(rest));
  return out.join('');
}

// The card that says what the part of a marker under the mouse means
const tipEl = document.body.appendChild(el('div', 'marker-tip'));

function showTip(target, { title, side, body }) {
  const sideLabel = side === 'doc' ? '<span class="tip-side doc">doc side</span>' : side === 'code' ? '<span class="tip-side">code side</span>' : '';
  tipEl.innerHTML = `<div class="tip-head"><code>${esc(title)}</code>${sideLabel}</div><p>${body}</p>`;
  tipEl.classList.add('shown');
  const r = target.getBoundingClientRect();
  const w = tipEl.offsetWidth, h = tipEl.offsetHeight;
  const left = Math.min(Math.max(8, r.left), window.innerWidth - w - 8);
  const top = r.bottom + 6 + h > window.innerHeight ? r.top - h - 6 : r.bottom + 6;
  tipEl.style.left = `${left}px`;
  tipEl.style.top = `${top}px`;
}

function hideTip() {
  tipEl.classList.remove('shown');
}

// Colors a marker line's parts, and explains each when hovered
function decorateMarkerLine(row) {
  const tips = [];
  const html = markerLineHtml(row.textContent, tips);
  if (html == null) return;
  row.innerHTML = html;
  for (const part of row.querySelectorAll('[data-tip]')) {
    part.addEventListener('mouseenter', () => showTip(part, tips[part.dataset.tip]));
    part.addEventListener('mouseleave', hideTip);
  }
}

// A whole file, with a range of lines highlighted and scrolled into view, and
// the marker lines around it emphasized. The file renders more than once
// (plain, then highlighted when its language loads), so each render's marker
// lines get decorated, and it's scrolled to them again as its layout settles,
// until the user scrolls it.
function renderFile(mount, name, contents, lines, markerLines = []) {
  let userScrolled = false;
  const scrollToMarkers = () => {
    const root = mount.querySelector('*')?.shadowRoot;
    const target = root?.querySelector(`[data-line="${markerLines[0] ?? lines?.[0]}"]`);
    if (userScrolled || !target) return;
    mount.scrollTop += target.getBoundingClientRect().top - mount.getBoundingClientRect().top - 40;
  };
  const decorate = (node, _file, phase) => {
    if (phase === 'unmount') return;
    const root = node.shadowRoot ?? node;
    for (const row of markerLines.flatMap(n => [...root.querySelectorAll(`[data-line="${n}"]`)])) {
      if (row.classList.contains('dac-marker')) continue;
      row.classList.add('dac-marker');
      decorateMarkerLine(row);
    }
    scrollToMarkers();
  };
  try {
    const f = new File({ theme: THEME, themeType: 'system', overflow: 'wrap', disableFileHeader: true, unsafeCSS: MARKER_CSS, onPostRender: decorate });
    f.render({ file: { name, contents }, containerWrapper: mount });
    rendered.push(f);
    if (lines) f.setSelectedLines({ start: lines[0], end: lines[1] });
    for (const event of ['wheel', 'pointerdown', 'keydown', 'touchstart']) mount.addEventListener(event, () => { userScrolled = true; }, { once: true });
    mount.addEventListener('scroll', hideTip);
    const resizes = new ResizeObserver(scrollToMarkers);
    resizes.observe(mount.firstElementChild);
    rendered.push({ cleanUp: () => resizes.disconnect() });
  } catch {
    mount.appendChild(el('pre', 'fallback', esc(contents)));
  }
}

function renderDiff(mount, docName, doc, repoName, repo) {
  const oldFile = { name: docName, contents: doc };
  const newFile = { name: repoName, contents: repo };
  try {
    const d = new FileDiff({ theme: THEME, themeType: 'system', diffStyle: 'split', expandUnchanged: true, overflow: 'wrap', lineDiffType: 'word' });
    d.render({ fileDiff: parseDiffFromFile(oldFile, newFile), oldFile, newFile, containerWrapper: mount });
    rendered.push(d);
  } catch {
    mount.appendChild(el('pre', 'fallback', esc(repo)));
  }
}

// --- Data ---

async function load() {
  const res = await fetch('/api/data');
  data = await res.json();
  for (const c of data.code) c.key = `code:${c.id}`;
}

async function act(path, body, button) {
  button?.classList.add('busy');
  try {
    const res = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    const result = await res.json();
    if (result.error) { toast(result.error, 'error'); return; }
    // Move on to the block that takes this one's place in the list
    const idx = visibleBlocks().findIndex(b => keyOf(b) === selectedKey);
    await load();
    const blocks = visibleBlocks();
    const next = blocks.find(b => keyOf(b) === selectedKey) ? selectedKey : (blocks[Math.min(idx, blocks.length - 1)] && keyOf(blocks[Math.min(idx, blocks.length - 1)]));
    select(next || null);
    toast('Saved', 'ok');
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    button?.classList.remove('busy');
  }
}

// Re-reads both repos. When the block on screen got resolved meanwhile (by an
// edit elsewhere), says so and moves on to the one that took its place.
async function rescan() {
  const listState = () => JSON.stringify([allBlocks().map(keyOf), resolvedBlocks().map(b => [keyOf(b), howResolved(b)]), ignoredBlocks().map(b => [keyOf(b), b.ignoredAs]), data.staleIgnored, data.code.map(c => [c.id, c.matchedBy.length]), data.problems, data.todos]);
  const before = visibleBlocks();
  const idx = before.findIndex(b => keyOf(b) === selectedKey);
  const shown = () => JSON.stringify([selectedBlock(), data.ignoreReasons]);
  const shownBefore = shown();
  const queueBefore = listState();
  await load();
  const blocks = visibleBlocks();
  if (selectedKey && idx !== -1 && !blocks.some(b => keyOf(b) === selectedKey)) {
    // A resolved or ignored block that moved to another tab: follow it there
    if (tab === 'resolved' || tab === 'ignored') {
      const has = list => list.some(b => keyOf(b) === selectedKey);
      const now = has(allBlocks()) ? 'todo' : has(resolvedBlocks()) ? 'resolved' : has(ignoredBlocks()) ? 'ignored' : null;
      if (now) { tab = now; select(selectedKey); return; }
    }
    toast(tab === 'todo' ? `✓ ${selectedKey.split(':')[1]} is resolved`
      : tab === 'code' || tab === 'todos' ? `${selectedKey.slice(5)} is no longer marked` : `${selectedKey.split(':')[1]} is gone`, 'ok');
    const next = blocks[Math.min(Math.max(idx, 0), blocks.length - 1)];
    select(next ? keyOf(next) : null);
    return;
  }
  if (!selectedKey && blocks[0]) selectedKey = keyOf(blocks[0]);
  // Leave what's on screen alone (scroll positions included) unless it changed
  if (listState() !== queueBefore) renderQueue();
  if (shown() !== shownBefore) {
    const scrollTop = blockEl.scrollTop;
    renderBlock();
    blockEl.scrollTop = scrollTop;
  }
}

// --- Queue ---

function renderQueue() {
  const remaining = allBlocks().length;
  headEl.innerHTML = `
    <div class="title">Docs ↔ Code</div>
    <div class="progress">
      <span><b>${remaining}</b> of ${data.total} doc blocks to resolve</span>
      <button class="link" title="Re-read both repos">Rescan</button>
    </div>
    <div class="bar"><div style="width:${data.total ? (100 * (data.total - remaining)) / data.total : 100}%"></div></div>`;
  headEl.querySelector('button').addEventListener('click', rescan);
  const tabLabels = {
    todo: ['To resolve', remaining], resolved: ['Resolved', resolvedBlocks().length],
    ignored: ['Ignored', ignoredBlocks().length], code: ['Marked code', data.code.length],
    todos: ['TODOs', data.todos.length],
  };
  queueEl.querySelector('.tabs').innerHTML = CATEGORIES.map(c => `
    <div class="tab-category">${c.label}</div>
    <div class="tab-row">${c.tabs.map(t => `<button data-tab="${t}" class="${t === tab ? 'active' : ''}">${tabLabels[t][0]} <span class="count">${tabLabels[t][1]}</span></button>`).join('')}</div>`).join('');

  listEl.innerHTML = '';
  const list = listEl;
  if (tab === 'code') { renderCodeList(list); return; }
  if (tab === 'todos') { renderTodoList(list); return; }
  let shown = 0;
  for (const g of data.guides) {
    const blocks = { todo: g.blocks, resolved: g.resolved, ignored: g.ignored }[tab].filter(matchesQuery);
    if (!blocks.length) continue;
    list.appendChild(el('div', 'guide', `<span>${esc(guideLabel(g))}</span><span class="count">${blocks.length}</span>`));
    for (const b of blocks) {
      shown++;
      const item = el('button', 'item' + (keyOf(b) === selectedKey ? ' selected' : ''));
      let badge = '';
      if (tab === 'todo') {
        badge = b.candidates[0]?.plan || b.formerly?.length
          ? '<span class="dot fix" title="A lightbulb can resolve it">💡</span>' : '';
      }
      item.innerHTML = `
        <span class="item-main"><code>${esc(preview(b.content))}</code>${badge}</span>
        <span class="item-sub">${esc(tab === 'todo' ? b.ref : howResolved(b))}</span>`;
      item.addEventListener('click', () => select(keyOf(b)));
      list.appendChild(item);
    }
  }
  if (!shown) {
    list.appendChild(el('div', query ? 'note' : 'all-done',
      query ? 'Nothing matches the search.' : { todo: 'Every doc block is resolved or ignored.', resolved: 'Nothing is resolved yet.', ignored: 'Nothing is ignored.' }[tab]));
  }
  if (tab !== 'todo') return;

  if (data.staleIgnored.length) {
    list.appendChild(el('div', 'guide aux', `Stale ignore entries <span class="count">${data.staleIgnored.length}</span>`));
    list.appendChild(el('div', 'note', 'Ignored content that no doc block has anymore.'));
    for (const c of data.staleIgnored) {
      const row = el('div', 'aux-row');
      row.innerHTML = `<span><code>${esc(preview(c.content))}</code><br><span class="muted">${esc(reasonLabel(c.reason))}</span></span>`;
      const del = el('button', 'link', 'Remove');
      del.addEventListener('click', () => act('/api/remove-ignored', { content: c.content }, del));
      row.appendChild(del);
      list.appendChild(row);
    }
  }
  if (data.problems.length) {
    list.appendChild(el('div', 'guide aux', `Marker problems <span class="count">${data.problems.length}</span>`));
    list.appendChild(el('div', 'note', 'Markers in the repo that can’t be read. Their code isn’t recommended until fixed.'));
    for (const p of data.problems) {
      list.appendChild(el('div', 'aux-row problem', `<span><code>${esc(p.file)}</code><br>${esc(p.message)}</span>`));
    }
  }
}

// The TODOs tab: every `| TODO` on this repo's markers, by file; each opens
// the marked code it's on
function renderTodoList(list) {
  const todos = data.todos.filter(t => !query || `${t.file} ${t.text}`.toLowerCase().includes(query));
  let file = null;
  for (const t of todos) {
    if (t.file !== file) {
      file = t.file;
      list.appendChild(el('div', 'guide', `<span>${esc(file)}</span><span class="count">${todos.filter(o => o.file === file).length}</span>`));
    }
    const key = `code:${t.codeId}`;
    const code = data.code.find(c => c.id === t.codeId);
    const item = el('button', 'item' + (key === selectedKey ? ' selected' : ''));
    item.innerHTML = `
      <span class="item-main">${esc(t.text)}</span>
      <span class="item-sub">line ${t.line}${code?.snippet ? ` · § ${esc(code.snippet)}` : ''}</span>`;
    if (code) item.addEventListener('click', () => select(key));
    else item.disabled = true;
    list.appendChild(item);
  }
  if (!todos.length) list.appendChild(el('div', 'note', query ? 'Nothing matches the search.' : 'No marker has a <code>| TODO</code>.'));
}

// The marked code tab: unmatched first, then matched
function renderCodeList(list) {
  const code = data.code.filter(matchesQuery);
  const groups = [
    ['Unmatched', code.filter(c => !c.matchedBy.length)],
    ['Matched', code.filter(c => c.matchedBy.length)],
  ];
  for (const [title, items] of groups) {
    if (!items.length) continue;
    list.appendChild(el('div', 'guide', `<span>${title}</span><span class="count">${items.length}</span>`));
    for (const c of items) {
      const item = el('button', 'item' + (keyOf(c) === selectedKey ? ' selected' : ''));
      const blocks = c.matchedBy.length ? `<span class="item-count">${c.matchedBy.length} doc block${c.matchedBy.length > 1 ? 's' : ''}</span>` : '';
      item.innerHTML = `
        <span class="item-main"><code>${esc(c.file)}</code>${blocks}</span>
        <span class="item-sub">${c.snippet ? `§ ${esc(c.snippet)}` : 'whole file'}</span>`;
      item.addEventListener('click', () => select(keyOf(c)));
      list.appendChild(item);
    }
  }
  if (!code.length) list.appendChild(el('div', 'note', query ? 'Nothing matches the search.' : 'Nothing is marked yet.'));
}

function select(key) {
  selectedKey = key;
  selectedCandidate = -1;
  ignoreChoice = null;
  history.replaceState(null, '', key ? `#${encodeURIComponent(key)}` : location.pathname);
  renderQueue();
  renderBlock();
  listEl.querySelector('.item.selected')?.scrollIntoView({ block: 'nearest' });
}

// --- Block ---

// An assembly's title, after its docs source's name when there are several
function guideLabel(g) {
  return `${g.docsName} › ${g.title}`;
}

function aiPrompt(b) {
  return `\`asadoc check ${b.ref} || asadoc guide\`.
`;
}

// The rendered listing for a block: the seq-th source block of its language
function findRenderedBlock(container, b) {
  const code = container.querySelectorAll(`.listingblock code[data-lang="${b.lang}"]`)[b.seq - 1];
  return code?.closest('.listingblock') || null;
}

async function copyPrompt(b) {
  await navigator.clipboard.writeText(aiPrompt(b));
  toast('AI prompt copied to clipboard', 'ok');
}

// Copies the prompt from an inline mention, pointing out the header's Copy AI
// prompt button so it's found next time
async function copyPromptFromMention(b) {
  await copyPrompt(b);
  const button = blockEl.querySelector('.copy-prompt');
  if (!button) return;
  button.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  button.classList.remove('highlight');
  void button.offsetWidth; // restart the animation on repeat clicks
  button.classList.add('highlight');
  button.addEventListener('animationend', () => button.classList.remove('highlight'), { once: true });
}

// An option as written on a marker
function optionCode(o) {
  const v = o.value == null ? '' : typeof o.value === 'object' ? `: ${o.value.from} -&gt; ${o.value.to}` : `: "${esc(o.value)}"`;
  return `<code>| ${o.side === 'doc' ? 'doc ' : ''}${esc(o.key)}${v}</code>`;
}

// Line numbers (in the file) of the lines in from..to whose text starts with `prefix`
function linesStartingWith(o, from, to, prefix) {
  const lines = o.fileText.split('\n');
  const hits = [];
  for (let n = from; n <= to; n++) if (lines[n - 1]?.trimStart().startsWith(prefix)) hits.push(n);
  return hits;
}

const lineList = ns => (ns.length === 1 ? `line ${ns[0]}` : `lines ${ns.slice(0, -1).join(', ')} and ${ns[ns.length - 1]}`);

// What adding an option to a marker does, in words
function describeOption(opt, o, region) {
  const code = optionCode(opt);
  const side = opt.side === 'doc' ? 'the doc block' : 'the code';
  switch (opt.key) {
    case 'remove-lines-starting-with': {
      const hits = opt.side === 'doc' || !region ? [] : linesStartingWith(o, region[0], region[1], opt.value);
      return `Add ${code} to the start marker, so lines of ${side} starting with <code>${esc(opt.value)}</code>`
        + `${hits.length ? ` (${lineList(hits)})` : ''} are left out when comparing`;
    }
    case 'remove-blank-lines': return `Add ${code} to the start marker, so blank lines of ${side} are left out when comparing`;
    case 'strip-line-prefix': return `Add ${code} to the start marker, so <code>${esc(opt.value)}</code> at the start of lines of ${side} is ignored when comparing`;
    default: return `Add ${code} to the start marker`;
  }
}

// The repo changes a lightbulb makes, one step each
function describePlan(o) {
  const plan = o.plan;
  const where = o.name ? `section <code>${esc(o.name)}</code>` : 'the file';
  const section = plan.fixes.find(f => f.type === 'mark-section');
  const region = section ? [section.line, section.line + section.lines - 1] : o.lines;
  const steps = [];
  for (const f of plan.fixes) {
    switch (f.type) {
      case 'mark-section':
        steps.push(`Add start and end markers around lines ${region[0]}–${region[1]}, making them section <code>${esc(f.name)}</code>`);
        for (const opt of f.options || []) steps.push(describeOption(opt, o, region));
        break;
      case 'remove-dashes': steps.push(`Remove the <code>---</code> line at the top of ${where}`); break;
      case 'add-dashes': steps.push(`Add a <code>---</code> line at the top of ${where}`); break;
      case 'trailing-newline':
        if (f.had === 0) steps.push(`Add the missing newline at the end of ${where}`);
        else if (f.newlines < f.had) steps.push(`Remove the extra blank line${f.had - f.newlines > 1 ? 's' : ''} at the end of ${where}`);
        else steps.push(`Add ${f.newlines - f.had} blank line${f.newlines - f.had > 1 ? 's' : ''} at the end of ${where}`);
        break;
      default: steps.push(esc(f.type));
    }
  }
  for (const opt of plan.docOptions || []) steps.push(describeOption(opt, o, region));
  // Options go on the file marker when there's no section
  return steps.map(s => (section ? s : s.replace('the start marker', 'the marker')));
}

function stateLabel(o) {
  if (o.plan) return '<span class="st fix">💡 Fixable</span>';
  return o.similarity < 0.15 ? '<span class="st">Different</span>' : `<span class="st">${Math.round(o.similarity * 100)}% similar</span>`;
}

// Which part of the file a recommendation is
function partLabel(o) {
  const lines = o.lines ? `<span class="lines">lines ${o.lines[0]}–${o.lines[1]}</span>` : '';
  switch (o.kind) {
    case 'section': return `<span class="part-icon">§</span><code>${esc(o.name)}</code>${lines}`;
    case 'file': return '<span class="part-icon">§</span>Whole file';
    default: return `<span class="part-icon unmarked">§</span><span class="unmarked">Would become a section</span>${lines}`;
  }
}

// An open recommendation: what linking involves, how it differs, and where it sits in its file
function candidateBody(b, o) {
  const body = el('div', 'cand-body');
  if (o.plan) {
    const btn = el('button', 'primary', 'Apply fix');
    btn.addEventListener('click', () => act('/api/fix', { asm: b.asm, ref: b.ref, id: o.id }, btn));
    body.appendChild(btn);
    const steps = describePlan(o);
    body.appendChild(el('div', 'fix-note', `💡 This makes it identical to the doc block by changing the repo:<ul>${steps.map(x => `<li>${x}</li>`).join('')}</ul>`));
  } else {
    const note = el('div', 'miss-note');
    note.innerHTML = 'Not identical. Edit it until it reads exactly like the doc (for values the repo leaves open, like <code>&lt;NODES_MTU&gt;</code>, add <code>| param: "&lt;NODES_MTU&gt;"</code> to its marker: <a href="/guide" target="_blank">see how</a>), or ';
    const copy = el('button', 'link inline', 'copy a prompt for your AI assistant');
    copy.addEventListener('click', () => copyPromptFromMention(b));
    note.appendChild(copy);
    note.appendChild(document.createTextNode('.'));
    body.appendChild(note);
  }
  const diff = el('div', 'diff');
  body.appendChild(diff);
  renderDiff(diff, o.docOptions.length ? 'doc (doc options applied)' : 'doc', o.doc,
    o.kind === 'lines' ? `${o.file}, lines ${o.lines[0]}-${o.lines[1]}` : (o.name ? `${o.file}, section ${o.name}` : o.file) + (o.options.length ? ' (marker options applied)' : ''), o.content);
  const view = el('div', 'file-view');
  body.appendChild(view);
  renderFile(view, o.file, o.fileText, o.kind === 'section' || o.kind === 'lines' ? o.lines : null, o.markerLines);
  return body;
}

function blockHeader(b, guide, withPrompt) {
  const head = el('header', 'block-head');
  head.innerHTML = `
    <div class="crumbs">${esc(guideLabel(guide))}${b.section ? ` › ${esc(b.section)}` : ''}</div>
    <div class="head-row">
      <span class="ref">${esc(b.ref)}</span>
      ${b.link ? `<a href="${b.link}" target="_blank">Doc source ↗</a>` : ''}
      ${withPrompt ? '<button class="secondary copy-prompt" title="A prompt for your AI assistant to link this block">Copy AI prompt</button>' : ''}
    </div>`;
  head.querySelector('.copy-prompt')?.addEventListener('click', () => copyPrompt(b));
  return head;
}

// The block as the reader sees it, in its module rendered with Asciidoctor
function showDocPreview(b) {
  const preview = el('div', 'doc-preview');
  blockEl.appendChild(preview);
  renderModule(b.asm, b.module).then(html => {
    if (selectedBlock() !== b) return;
    if (!html) {
      preview.className = 'doc-code';
      renderCode(preview, `${b.module}.${b.lang}`, b.content.replace(/\n$/, ''));
      return;
    }
    preview.innerHTML = html;
    const target = findRenderedBlock(preview, b);
    if (target) {
      target.classList.add('this-block');
      requestAnimationFrame(() => { preview.scrollTop = target.offsetTop - preview.offsetTop - 60; });
    }
  });
}

const renderedModules = new Map();

// A module rendered with the attributes its assembly gives it (a trailing @
// lets the module override them)
function renderModule(asm, module) {
  const key = `${asm}/${module}`;
  if (!renderedModules.has(key)) {
    renderedModules.set(key, fetch(`/api/module?asm=${encodeURIComponent(asm)}&module=${encodeURIComponent(module)}`)
      .then(r => (r.ok ? r.json() : null))
      .then(m => {
        if (!m) return null;
        const attributes = Object.fromEntries(Object.entries(m.attributes).map(([k, v]) => [k, `${v}@`]));
        return asciidoctor.convert(m.text, { safe: 'safe', attributes: { ...attributes, showtitle: true } });
      })
      .catch(() => null));
  }
  return renderedModules.get(key);
}

// An ignored block: why, and the way back to the queue
function renderIgnored(b, guide) {
  blockEl.appendChild(blockHeader(b, guide, false));
  showDocPreview(b);
  const sec = el('section', 'resolved');
  const head = el('div', 'resolved-head', `<span class="st">Ignored</span> as <b>${esc(reasonLabel(b.ignoredAs))}</b>`);
  const btn = el('button', 'secondary danger', 'Stop ignoring');
  btn.title = 'Delete its file from .asadoc/ignore/ and send it back to the queue';
  btn.addEventListener('click', () => act('/api/unignore', { asm: b.asm, ref: b.ref }, btn));
  head.appendChild(btn);
  sec.appendChild(head);
  blockEl.appendChild(sec);
}

// A resolved block: the code that matches it, in its file
function renderResolved(b, guide) {
  blockEl.appendChild(blockHeader(b, guide, false));
  showDocPreview(b);

  const sec = el('section', 'resolved');
  for (const c of b.code) {
    const where = c.snippet
      ? `<code>${esc(c.file)}</code><span class="sep">›</span><span class="part-icon">§</span><code>${esc(c.snippet)}</code><span class="lines">lines ${c.lines[0]}–${c.lines[1]}</span>`
      : `<code>${esc(c.file)}</code><span class="lines">whole file</span>`;
    const head = el('div', 'resolved-head', '<span class="st exact">✓ Resolved</span> Matches ');
    const link = el('button', 'link code-link', where);
    link.title = 'Open this marked code';
    link.addEventListener('click', () => goToCode(c.id));
    head.appendChild(link);
    sec.appendChild(head);
    // What the doc put where the code has placeholders
    const values = Object.entries(c.values || {});
    if (values.length) {
      sec.appendChild(el('table', 'param-values', `
        <thead><tr><th>Param in the repo</th><th>Value in the doc</th></tr></thead>
        <tbody>${values.map(([p, v]) => `<tr><td><code>${esc(p)}</code></td><td><code>${v ? esc(v) : '<span class="muted">(empty)</span>'}</code></td></tr>`).join('')}</tbody>`));
    }
    const view = el('div', 'file-view');
    sec.appendChild(view);
    fetchText(fileCache, `/api/file?path=${encodeURIComponent(c.file)}`).then(text => {
      if (text == null || selectedBlock() !== b) return;
      renderFile(view, c.file, text, c.snippet ? c.lines : null, c.markerLines);
    });
  }
  blockEl.appendChild(sec);
}

// A piece of marked code: which doc blocks it matches (or resembles), and where it sits in its file
function renderMarkedCode(c) {
  const where = c.snippet
    ? `<span class="sep">›</span><span class="part-icon">§</span><code>${esc(c.snippet)}</code><span class="lines">lines ${c.lines[0]}–${c.lines[1]}</span>`
    : '<span class="lines">whole file</span>';
  blockEl.appendChild(el('header', 'block-head', `
    <div class="crumbs">Marked code</div>
    <div class="head-row">
      <span class="ref">${esc(c.file)}</span>${where}
      ${c.link ? `<a href="${c.link}" target="_blank">Source ↗</a>` : ''}
      <button class="secondary remove-markers" title="Delete this code's markers, leaving the code itself">Remove markers</button>
    </div>`));
  blockEl.querySelector('.remove-markers').addEventListener('click', (e) => {
    const n = c.matchedBy.length;
    if (n && !confirm(`${codeName(c)} matches ${n} doc block${n > 1 ? 's' : ''} (${c.matchedBy.map(m => m.ref).join(', ')}). `
      + `Without its markers, ${n > 1 ? 'they go' : 'it goes'} back to the blocks to resolve. Remove the markers anyway?`)) return;
    act('/api/remove-markers', { id: c.id }, e.currentTarget);
  });

  const sec = el('section', 'resolved');
  const blockLink = (asm, ref, extra = '') => {
    const guide = data.guides.find(g => g.id === asm);
    const btn = el('button', 'block-link', `<code>${esc(ref)}</code><span class="muted">${esc(guide ? guideLabel(guide) : asm)}</span>${extra}`);
    btn.addEventListener('click', () => goToBlock(asm, ref));
    return btn;
  };
  if (c.matchedBy.length) {
    sec.appendChild(el('div', 'resolved-head', `<span class="st exact">✓ Matched</span> by ${c.matchedBy.length} doc block${c.matchedBy.length > 1 ? 's' : ''}`));
    for (const m of c.matchedBy) {
      sec.appendChild(blockLink(m.asm, m.ref));
    }
  } else {
    sec.appendChild(el('div', 'resolved-head', '<span class="st">Unmatched</span> No doc block matches it.'));
    if (c.resembledBy.length) {
      sec.appendChild(el('p', 'muted small', 'Unresolved doc blocks that resemble it:'));
      for (const r of c.resembledBy) sec.appendChild(blockLink(r.asm, r.ref, `<span class="muted">${Math.round(r.similarity * 100)}% similar</span>`));
    } else {
      sec.appendChild(el('p', 'muted small', 'No doc block resembles it.'));
    }
  }
  const view = el('div', 'file-view');
  sec.appendChild(view);
  fetchText(fileCache, `/api/file?path=${encodeURIComponent(c.file)}`).then(text => {
    if (text == null || selectedBlock() !== c) return;
    renderFile(view, c.file, text, c.snippet ? c.lines : null, c.markerLines);
  });
  blockEl.appendChild(sec);
}

// Fetched on demand for resolved blocks
const fileCache = new Map();

async function fetchText(cache, url) {
  if (!cache.has(url)) cache.set(url, fetch(url).then(r => (r.ok ? r.text() : null)));
  return cache.get(url);
}

function renderBlock() {
  cleanup();
  blockEl.innerHTML = '';
  const b = selectedBlock();
  if (!b) {
    blockEl.appendChild(el('div', 'empty', visibleBlocks().length ? 'Pick a doc block from the list.' : tab === 'todo' ? 'Nothing to resolve.' : 'Nothing to show.'));
    return;
  }
  if (isMarkedCode(b)) { renderMarkedCode(b); return; }
  const guide = data.guides.find(g => g.id === b.asm);
  if (b.ignoredAs) { renderIgnored(b, guide); return; }
  if (isResolved(b)) { renderResolved(b, guide); return; }

  blockEl.appendChild(blockHeader(b, guide, true));

  showDocPreview(b);

  // An ignored block whose content changed: offer to ignore the new content in its place
  for (const f of b.formerly || []) {
    const note = el('div', 'previous', `💡 This looks like a block ignored as <b>${esc(reasonLabel(f.reason))}</b> before it changed. `);
    const btn = el('button', 'link inline', `Ignore it again as ${reasonLabel(f.reason)}`);
    btn.title = 'Ignore the new content, replacing the old entry';
    btn.addEventListener('click', () => act('/api/ignore', { asm: b.asm, ref: b.ref, reason: f.reason, replacing: f.content }, btn));
    note.appendChild(btn);
    blockEl.appendChild(note);
  }

  // Associate
  const assoc = el('section', 'associate');
  assoc.appendChild(el('h2', null, 'Make repo code match'));
  if (!b.candidates.length) {
    const none = el('p', 'muted', data.code.length
      ? 'No currently marked code resembles this block. Mark the code it comes from ('
      : 'No code is marked yet. Mark the code this block comes from (');
    const copy = el('button', 'link inline', 'or have your AI assistant do it');
    copy.title = 'Copy a prompt for your AI assistant to link this block';
    copy.addEventListener('click', () => copyPromptFromMention(b));
    none.append(copy, ').');
    assoc.appendChild(none);
  }
  b.candidates.forEach((o, i) => {
    const open = i === selectedCandidate;
    const card = el('div', 'candidate' + (open ? ' open' : '') + (o.plan ? ' fix' : ''));
    const summary = el('button', 'cand-head');
    summary.innerHTML = `
      <span class="cand-key">${i + 1}</span>
      <span class="cand-name"><code>${esc(o.file)}</code><span class="sep">›</span>${partLabel(o)}</span>
      <span class="cand-state">${stateLabel(o)}</span>`;
    summary.addEventListener('click', () => { selectedCandidate = open ? -1 : i; renderBlock(); });
    card.appendChild(summary);
    if (open) card.appendChild(candidateBody(b, o));
    assoc.appendChild(card);
  });
  blockEl.appendChild(assoc);

  blockEl.appendChild(ignoreSection(b));
}

// Ignoring the block: pick a reason (or describe a new one), then apply it
function ignoreSection(b) {
  ignoreChoice ??= { reason: data.ignoreReasons.length ? null : NEW_REASON, name: '', description: '' };
  const ign = el('section', 'ignore');
  ign.appendChild(el('h2', null, 'Or ignore it'));
  const form = el('form', 'ignore-form');
  const option = (reason, title, description) => {
    const label = el('label', 'ignore-option' + (ignoreChoice.reason === reason ? ' checked' : ''));
    const radio = el('input');
    radio.type = 'radio';
    radio.name = 'reason';
    radio.checked = ignoreChoice.reason === reason;
    radio.addEventListener('change', () => { ignoreChoice.reason = reason; renderIgnoreForm(); });
    label.appendChild(radio);
    const text = el('span', 'ignore-text', `<b>${title}</b>`);
    text.appendChild(el('span', 'ignore-desc', description));
    label.appendChild(text);
    return label;
  };
  for (const r of data.ignoreReasons) {
    form.appendChild(option(r.name, esc(reasonLabel(r.name)), r.description
      ? esc(r.description)
      : `<span class="muted">No description: add one to <code>.asadoc/ignore/${esc(r.name)}/README.md</code></span>`));
  }
  const fresh = option(NEW_REASON, 'New reason…', '<span class="muted">Name a reason that isn’t listed, and say what it means</span>');
  form.appendChild(fresh);
  const fields = el('div', 'new-reason');
  const name = el('input');
  name.placeholder = 'Name, e.g. Example output';
  name.value = ignoreChoice.name;
  const where = el('div', 'muted small');
  const description = el('textarea');
  description.rows = 2;
  description.placeholder = 'What it means, e.g. Sample output shown to the reader, not produced by this repo';
  description.value = ignoreChoice.description;
  fields.append(name, where, description);
  fresh.querySelector('.ignore-text').appendChild(fields);
  const apply = el('button', 'primary');
  apply.type = 'submit';
  form.appendChild(apply);
  ign.appendChild(form);

  // Only the parts that depend on the choice, so typing keeps focus
  function renderIgnoreForm() {
    const isNew = ignoreChoice.reason === NEW_REASON;
    const slug = reasonName(ignoreChoice.name);
    const taken = data.ignoreReasons.some(r => r.name === slug);
    for (const label of form.querySelectorAll('.ignore-option')) label.classList.toggle('checked', label.querySelector('input').checked);
    fields.hidden = !isNew;
    where.innerHTML = !slug ? '&nbsp;'
      : taken ? `<span class="error">${esc(reasonLabel(slug))} already exists: pick it above</span>`
      : `Saved in <code>.asadoc/ignore/${esc(slug)}/</code>`;
    const ready = isNew ? slug && !taken && ignoreChoice.description.trim() : ignoreChoice.reason != null;
    apply.disabled = !ready;
    apply.textContent = !ready ? 'Ignore' : `Ignore as ${reasonLabel(isNew ? slug : ignoreChoice.reason)}`;
  }
  name.addEventListener('input', () => { ignoreChoice.name = name.value; renderIgnoreForm(); });
  description.addEventListener('input', () => { ignoreChoice.description = description.value; renderIgnoreForm(); });
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    if (apply.disabled) return;
    const body = ignoreChoice.reason === NEW_REASON
      ? { asm: b.asm, ref: b.ref, reason: reasonName(ignoreChoice.name), newReason: ignoreChoice.description.trim() }
      : { asm: b.asm, ref: b.ref, reason: ignoreChoice.reason };
    act('/api/ignore', body, apply);
  });
  renderIgnoreForm();
  return ign;
}

// --- Keyboard: j/k move through the queue, 1-9 open a candidate ---

document.addEventListener('keydown', (e) => {
  if (e.target.closest('input, textarea') || e.metaKey || e.ctrlKey || e.altKey) return;
  const blocks = visibleBlocks();
  const idx = blocks.findIndex(b => keyOf(b) === selectedKey);
  if ((e.key === 'j' || e.key === 'ArrowDown') && idx < blocks.length - 1) { select(keyOf(blocks[idx + 1])); e.preventDefault(); }
  else if ((e.key === 'k' || e.key === 'ArrowUp') && idx > 0) { select(keyOf(blocks[idx - 1])); e.preventDefault(); }
  else if (/^[1-9]$/.test(e.key)) {
    const i = parseInt(e.key, 10) - 1;
    if (selectedBlock()?.candidates?.[i]) { selectedCandidate = i; renderBlock(); }
  }
});

// Edits made elsewhere (by hand or by an assistant) show up as they happen.
// Reconnecting means the server restarted, possibly with new code: reload.
const events = new EventSource('/api/events');
let lostServer = false;
events.onmessage = () => rescan();
events.onerror = () => { lostServer = true; };
events.onopen = () => { if (lostServer) location.reload(); };

// Until the server has the review UI ready (it serves right away, and loads
// the config and evaluates in the background), says what it's busy with.
// Returns the screen still up (or null), for taking down once the page shows.
async function untilReady() {
  let screen = null;
  for (;;) {
    let status;
    try {
      status = await (await fetch('/api/status')).json();
    } catch {
      status = { state: 'preparing', step: 'waiting for the asadoc server', seconds: null };
    }
    if (status.state === 'ready') return screen;
    if (!screen) screen = document.body.appendChild(el('div', 'preparing'));
    if (status.state === 'failed') {
      screen.innerHTML = `<div class="preparing-box failed">
        <div class="title">asadoc couldn't prepare the review UI</div>
        <pre>${esc(status.error)}</pre>
        <p class="muted">Fix this, then restart <code>asadoc serve</code>.</p></div>`;
      return new Promise(() => {});
    }
    const elapsed = status.seconds == null ? '' : ` <span class="muted">· ${status.seconds}s</span>`;
    screen.innerHTML = `<div class="preparing-box">
      <div class="title"><span class="spinner"></span>Preparing the review UI…</div>
      <p>${esc(status.step || 'starting')}${elapsed}</p>
      <p class="muted">asadoc is reading the docs and the code. The first run fetches the docs, which
        can take a while. This page opens by itself when it's ready.</p></div>`;
    await new Promise(r => setTimeout(r, 500));
  }
}

(async () => {
  const screen = await untilReady();
  screen?.querySelector('p')?.replaceChildren('loading the review');
  await load();
  const fromHash = decodeURIComponent(location.hash.slice(1));
  if (resolvedBlocks().some(b => keyOf(b) === fromHash)) tab = 'resolved';
  if (ignoredBlocks().some(b => keyOf(b) === fromHash)) tab = 'ignored';
  if (data.code.some(c => keyOf(c) === fromHash)) tab = 'code';
  selectedKey = tabBlocks().some(b => keyOf(b) === fromHash) ? fromHash : (tabBlocks()[0] ? keyOf(tabBlocks()[0]) : null);
  renderQueue();
  renderBlock();
  screen?.remove();
})();
