// The UI only presents what the asadoc server computes, and asks it for changes.
import { FileDiff, File, parseDiffFromFile } from 'https://esm.sh/@pierre/diffs@1.4.3';
import Asciidoctor from 'https://esm.sh/@asciidoctor/core@3.0.4';

const asciidoctor = Asciidoctor();

// A queue of doc blocks that no marked repo code matches yet. Each one gets
// resolved by making some code match it (by hand, by an AI assistant, or with
// a lightbulb fix), or ignored when no repo code should match it. Resolved and
// ignored blocks, and all marked code, can be browsed too.

const IGNORE_REASONS = [
  { reason: 'example-output', label: 'Example output', hint: 'Sample output shown to the reader' },
  { reason: 'manual-command', label: 'Manual command', hint: 'A command too simple or doc-specific to track' },
  { reason: 'no-repo-source', label: 'No repo source', hint: 'Content with no counterpart in this repo' },
];

let data = null;
let selectedKey = null;   // `${asm}:${ref}`
let tab = 'todo';         // 'todo' | 'resolved' | 'ignored' | 'code'
let query = '';
let selectedCandidate = 0;    // index of the open recommendation
let rendered = [];        // pierre components to clean up

const root = document.getElementById('root');
root.innerHTML = `
  <aside id="queue">
    <div class="queue-head"></div>
    <div class="queue-controls">
      <div class="tabs">
        <button data-tab="todo"></button>
        <button data-tab="resolved"></button>
        <button data-tab="ignored"></button>
        <button data-tab="code"></button>
      </div>
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
  return { todo: allBlocks, resolved: resolvedBlocks, ignored: ignoredBlocks, code: () => data.code }[tab]();
}

// Doc blocks are keyed by guide and ref, marked code by its id
function keyOf(b) {
  return b.key || `${b.asm}:${b.ref}`;
}

const isMarkedCode = b => !!b.key;

function codeName(c) {
  return c.snippet ? `${c.file} › § ${c.snippet}` : `${c.file} (whole file)`;
}

function reasonLabel(reason) {
  return IGNORE_REASONS.find(r => r.reason === reason)?.label || reason;
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

// Marker lines stand out in the file view, with the section name emphasized
const MARKER_CSS = `
  [data-line].dac-marker, [data-line].dac-marker [data-column-content], [data-line].dac-marker * {
    color: var(--dac-marker-fg) !important;
  }
  [data-line].dac-marker { background: var(--dac-marker-bg) !important; font-weight: 600; }
  [data-line].dac-marker mark.dac-name, [data-line].dac-marker mark.dac-name * {
    background: var(--dac-name-bg); color: var(--dac-name-fg) !important;
    border-radius: 3px; padding: 0 3px;
  }
`;

// A whole file, with a range of lines highlighted and scrolled into view, and
// the marker lines around it (and the section name in them) emphasized
function renderFile(mount, name, contents, lines, markerLines = [], sectionName = null) {
  try {
    const f = new File({ theme: THEME, themeType: 'system', overflow: 'wrap', disableFileHeader: true, unsafeCSS: MARKER_CSS });
    f.render({ file: { name, contents }, containerWrapper: mount });
    rendered.push(f);
    if (lines) f.setSelectedLines({ start: lines[0], end: lines[1] });
    let tries = 0;
    const decorate = () => {
      const root = mount.querySelector('*')?.shadowRoot;
      const rows = root ? markerLines.flatMap(n => [...root.querySelectorAll(`[data-line="${n}"]`)]) : [];
      if (!rows.length && markerLines.length && tries++ < 20) { setTimeout(decorate, 100); return; }
      for (const row of rows) {
        row.classList.add('dac-marker');
        if (sectionName) markText(row, `"${sectionName}"`);
      }
      const target = root?.querySelector(`[data-line="${markerLines[0] ?? lines?.[0]}"]`);
      if (target) mount.scrollTop += target.getBoundingClientRect().top - mount.getBoundingClientRect().top - 40;
    };
    requestAnimationFrame(decorate);
  } catch {
    mount.appendChild(el('pre', 'fallback', esc(contents)));
  }
}

// Wraps the first occurrence of `needle` within an element's text in <mark>
function markText(root, needle) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const i = node.data.indexOf(needle);
    if (i === -1) continue;
    const mark = document.createElement('mark');
    mark.className = 'dac-name';
    const rest = node.splitText(i);
    rest.splitText(needle.length);
    rest.parentNode.replaceChild(mark, rest);
    mark.appendChild(rest);
    return;
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
  const listState = () => JSON.stringify([allBlocks().map(keyOf), resolvedBlocks().map(b => [keyOf(b), howResolved(b)]), ignoredBlocks().map(b => [keyOf(b), b.ignoredAs]), data.staleIgnored, data.code.map(c => [c.id, c.matchedBy.length]), data.problems]);
  const before = visibleBlocks();
  const idx = before.findIndex(b => keyOf(b) === selectedKey);
  const shownBefore = JSON.stringify(selectedBlock());
  const queueBefore = listState();
  await load();
  const blocks = visibleBlocks();
  if (selectedKey && idx !== -1 && !blocks.some(b => keyOf(b) === selectedKey)) {
    toast(tab === 'todo' ? `✓ ${selectedKey.split(':')[1]} is resolved`
      : tab === 'code' ? `${selectedKey.slice(5)} is no longer marked` : `${selectedKey.split(':')[1]} is back in the queue`, 'ok');
    const next = blocks[Math.min(Math.max(idx, 0), blocks.length - 1)];
    select(next ? keyOf(next) : null);
    return;
  }
  if (!selectedKey && blocks[0]) selectedKey = keyOf(blocks[0]);
  // Leave what's on screen alone (scroll positions included) unless it changed
  if (listState() !== queueBefore) renderQueue();
  if (JSON.stringify(selectedBlock()) !== shownBefore) {
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
  for (const btn of queueEl.querySelectorAll('.tabs button')) {
    const t = btn.dataset.tab;
    btn.className = t === tab ? 'active' : '';
    const [label, count] = {
      todo: ['To resolve', remaining], resolved: ['Resolved', resolvedBlocks().length],
      ignored: ['Ignored', ignoredBlocks().length], code: ['Marked code', data.code.length],
    }[t];
    btn.innerHTML = `${label} <span class="count">${count}</span>`;
  }

  listEl.innerHTML = '';
  const list = listEl;
  if (tab === 'code') { renderCodeList(list); return; }
  let shown = 0;
  for (const g of data.guides) {
    const blocks = { todo: g.blocks, resolved: g.resolved, ignored: g.ignored }[tab].filter(matchesQuery);
    if (!blocks.length) continue;
    list.appendChild(el('div', 'guide', `<span>${esc(g.title)}</span><span class="count">${blocks.length}</span>`));
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
  selectedCandidate = 0;
  history.replaceState(null, '', key ? `#${encodeURIComponent(key)}` : location.pathname);
  renderQueue();
  renderBlock();
  listEl.querySelector('.item.selected')?.scrollIntoView({ block: 'nearest' });
}

// --- Block ---

function aiPrompt(b) {
  return `In this repo, make marked code match this docs code block. Run \`asadoc guide\` first to learn how markers work.

  ${b.ref}: modules/${b.module}.adoc, line ${b.line}, in the docs at ${data.docsLocation}

Don't edit the docs.
Check with \`asadoc check ${b.ref}\`: it says why the block isn't resolved, with a diff. You're done when it reports ✓ resolved; show that output.
`;
}

// The rendered listing for a block: the seq-th source block of its language
function findRenderedBlock(container, b) {
  const code = container.querySelectorAll(`.listingblock code[data-lang="${b.lang}"]`)[b.seq - 1];
  return code?.closest('.listingblock') || null;
}

async function copyPrompt(b, button) {
  await navigator.clipboard.writeText(aiPrompt(b));
  const text = button.textContent;
  button.textContent = 'Copied';
  setTimeout(() => { button.textContent = text; }, 1500);
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
      case 'mark-file': steps.push('Add a <code># @docs-as-code: file</code> line at the top, marking the whole file'); break;
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
    case 'lines': return `<span class="part-icon unmarked">§</span><span class="unmarked">Would become a section</span>${lines}`;
    default: return '<span class="part-icon unmarked">§</span><span class="unmarked">Whole file, not marked</span>';
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
    copy.addEventListener('click', () => copyPrompt(b, copy));
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
  renderFile(view, o.file, o.fileText, o.kind === 'section' || o.kind === 'lines' ? o.lines : null, o.markerLines, o.name);
  return body;
}

function blockHeader(b, guide, withPrompt) {
  const head = el('header', 'block-head');
  head.innerHTML = `
    <div class="crumbs">${esc(guide.title)}${b.section ? ` › ${esc(b.section)}` : ''}</div>
    <div class="head-row">
      <span class="ref">${esc(b.ref)}</span>
      ${data.links.docs ? `<a href="${data.links.docs}modules/${b.module}.adoc?plain=1#L${b.line}" target="_blank">Doc source ↗</a>` : ''}
      ${withPrompt ? '<button class="secondary copy-prompt" title="A prompt for your AI assistant to link this block">Copy AI prompt</button>' : ''}
    </div>`;
  head.querySelector('.copy-prompt')?.addEventListener('click', (e) => copyPrompt(b, e.target));
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
  btn.title = 'Delete its file from .asadoc-ignore/ and send it back to the queue';
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
    sec.appendChild(el('div', 'resolved-head', `<span class="st exact">✓ Resolved</span> Matches ${where}`));
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
      renderFile(view, c.file, text, c.snippet ? c.lines : null, c.markerLines, c.snippet);
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
      ${data.links.repo ? `<a href="${data.links.repo}${c.file}${c.lines ? `#L${c.lines[0]}-L${c.lines[1]}` : ''}" target="_blank">Source ↗</a>` : ''}
    </div>`));

  const sec = el('section', 'resolved');
  const blockLink = (asm, ref, extra = '') => {
    const guide = data.guides.find(g => g.id === asm);
    const btn = el('button', 'block-link', `<code>${esc(ref)}</code><span class="muted">${esc(guide?.title || asm)}</span>${extra}`);
    btn.addEventListener('click', () => goToBlock(asm, ref));
    return btn;
  };
  if (c.matchedBy.length) {
    sec.appendChild(el('div', 'resolved-head', `<span class="st exact">✓ Matched</span> by ${c.matchedBy.length} doc block${c.matchedBy.length > 1 ? 's' : ''}`));
    for (const m of c.matchedBy) {
      sec.appendChild(blockLink(m.asm, m.ref));
      const values = Object.entries(m.values || {});
      if (values.length) {
        sec.appendChild(el('table', 'param-values', `
          <thead><tr><th>Param in the repo</th><th>Value in the doc</th></tr></thead>
          <tbody>${values.map(([p, v]) => `<tr><td><code>${esc(p)}</code></td><td><code>${v ? esc(v) : '<span class="muted">(empty)</span>'}</code></td></tr>`).join('')}</tbody>`));
      }
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
    renderFile(view, c.file, text, c.snippet ? c.lines : null, c.markerLines, c.snippet);
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
    assoc.appendChild(el('p', 'muted', 'Nothing in the repo looks like this block. Mark the code it comes from (or have your AI assistant do it).'));
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

  // Ignore
  const ign = el('section', 'ignore');
  ign.appendChild(el('h2', null, 'Or ignore it, as:'));
  const row = el('div', 'ignore-row');
  for (const r of IGNORE_REASONS) {
    const btn = el('button', 'secondary', esc(r.label));
    btn.title = r.hint;
    btn.addEventListener('click', () => act('/api/ignore', { asm: b.asm, ref: b.ref, reason: r.reason }, btn));
    row.appendChild(btn);
  }
  ign.appendChild(row);
  blockEl.appendChild(ign);
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

(async () => {
  await load();
  const fromHash = decodeURIComponent(location.hash.slice(1));
  if (resolvedBlocks().some(b => keyOf(b) === fromHash)) tab = 'resolved';
  if (ignoredBlocks().some(b => keyOf(b) === fromHash)) tab = 'ignored';
  if (data.code.some(c => keyOf(c) === fromHash)) tab = 'code';
  selectedKey = tabBlocks().some(b => keyOf(b) === fromHash) ? fromHash : (tabBlocks()[0] ? keyOf(tabBlocks()[0]) : null);
  renderQueue();
  renderBlock();
})();
