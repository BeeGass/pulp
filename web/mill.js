// pulp mill UI, shared by the local mill (pulp ui), the browser mill (/mill),
// and the site's product shot. Each surface passes an adapter that talks to its
// packer. Source of truth lives in web/; `cargo xtask site` copies it to site/mill/.

const ISSUES_URL = 'https://github.com/BeeGass/pulp/issues/new';
const REPO_URL = 'https://github.com/BeeGass/pulp';
const LARGE_TREE = 400;
const LEDGER_CHUNK = 400;
const FULL_TREE = 600;
const OVERSCAN = 30;
const SETTINGS_KEY = 'pulp.mill.settings';
const PATH_KEY = 'pulp.mill.path';

const DEFAULTS = {
  content: 'readable',
  format: 'xml',
  gitignore: true,
  hidden: false,
  archives: false,
  notebook: false,
  tree: true,
  wrap: false,
};

const ICONS = {
  lock: '<rect x="3.5" y="7" width="9" height="6.5" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/>',
  folder: '<path d="M1.75 4.25a1 1 0 0 1 1-1h3.1l1.5 1.5h5.9a1 1 0 0 1 1 1v6.5a1 1 0 0 1-1 1H2.75a1 1 0 0 1-1-1z"/>',
  file: '<path d="M4 1.75h5l3 3v9.5H4z"/><path d="M9 1.75v3h3"/>',
  files: '<rect x="4.5" y="1.75" width="8" height="10" rx="1"/><path d="M3.5 4.5v8.75a1 1 0 0 0 1 1H11"/>',
  copy: '<rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5v-2a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2"/>',
  download: '<path d="M8 2.5v8M4.75 7.25 8 10.5l3.25-3.25M3 13.5h10"/>',
  search: '<circle cx="7" cy="7" r="4.25"/><path d="m10.25 10.25 3.25 3.25"/>',
  press: '<path d="M5 3.25v9.5L12.5 8z"/>',
  tree: '<path d="M3 2.5v11M3 5.5h4M3 11h4"/><rect x="8" y="3.5" width="5" height="4" rx="1"/><rect x="8" y="9" width="5" height="4" rx="1"/>',
  chev: '<path d="m4 6 4 4 4-4"/>',
  alert: '<path d="M8 2.2 14.3 13.3H1.7z"/><path d="M8 6.5v3.2M8 11.4v.1"/>',
  x: '<path d="m4 4 8 8M12 4l-8 8"/>',
  sliders: '<path d="M2.5 4.5h6.5M12.5 4.5h1M2.5 11.5h1.5M7 11.5h6.5"/><circle cx="10.75" cy="4.5" r="1.75"/><circle cx="5.5" cy="11.5" r="1.75"/>',
  flask: '<path d="M6 2h4M6.5 2v4L3 13a1 1 0 0 0 .9 1.5h8.2A1 1 0 0 0 13 13L9.5 6V2"/>',
  check: '<path d="m3.5 8.5 3 3 6-7"/>',
  github: '<path d="M6 13.5c-3 1-3-1.5-4.5-2m9 3.5v-2.4a2.1 2.1 0 0 0-.6-1.6c2-.2 4.1-1 4.1-4.5a3.5 3.5 0 0 0-1-2.4 3.2 3.2 0 0 0-.1-2.4s-.8-.2-2.5 1a8.6 8.6 0 0 0-4.5 0C4.2 1.5 3.4 1.7 3.4 1.7a3.2 3.2 0 0 0-.1 2.4 3.5 3.5 0 0 0-1 2.4c0 3.5 2.1 4.3 4.1 4.5a2.1 2.1 0 0 0-.6 1.6V15"/>',
  tray: '<path d="M2 9.5v3a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1v-3"/><path d="M8 2.5v7M5 6.5l3 3 3-3"/>',
};

export function icon(name, cls) {
  return '<svg class="p-icon' + (cls ? ' ' + cls : '') + '" viewBox="0 0 16 16" aria-hidden="true" focusable="false">' +
    ICONS[name] + '</svg>';
}

function esc(value) {
  return String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

export function human(n) {
  const b = Number(n) || 0;
  if (b < 1024) return b + ' B';
  const units = ['KiB', 'MiB', 'GiB'];
  let value = b / 1024;
  let unit = 0;
  // Step up while the value would round to 1024.0 in this unit.
  while (unit < units.length - 1 && value >= 1023.95) {
    value /= 1024;
    unit++;
  }
  return value.toFixed(1) + ' ' + units[unit];
}

function tokensText(n) {
  const t = Number(n) || 0;
  return t < 1000 ? String(t) : (t / 1000).toFixed(1) + 'k';
}

function plural(n, one, many) {
  return n + ' ' + (n === 1 ? one : many || one + 's');
}

function fileId(f) {
  return f.id || f.relative;
}

function langOf(f) {
  return f.language || f.kind || 'file';
}

function readStore(key) {
  try {
    const raw = window.localStorage.getItem(key);
    return raw ? JSON.parse(raw) : null;
  } catch (_) {
    return null;
  }
}

function writeStore(key, value) {
  try {
    window.localStorage.setItem(key, JSON.stringify(value));
  } catch (_) {
    // Storage can be blocked; settings then last for this tab only.
  }
}

function isCancel(err) {
  return !!err && (err.name === 'AbortError' || err.cancelled === true || err.message === 'cancelled');
}

function errText(err) {
  return err && err.message ? err.message : String(err);
}

function fence(text) {
  const body = String(text || '');
  const runs = body.match(/`+/g) || [];
  const width = runs.reduce((max, run) => Math.max(max, run.length), 0) + 1;
  const ticks = '`'.repeat(Math.max(3, width));
  return ticks + 'text\n' + body + '\n' + ticks;
}

/**
 * Copy `text`, a string or a promise of one. Safari accepts a clipboard write
 * only inside the click that asked for it, so text that is still loading goes
 * to the clipboard as a promise instead of being awaited first.
 */
async function writeClipboard(text) {
  if (navigator.clipboard && window.isSecureContext) {
    if (typeof text !== 'string' && typeof ClipboardItem === 'function' && navigator.clipboard.write) {
      const pending = Promise.resolve(text);
      try {
        const blob = pending.then((t) => new Blob([t], { type: 'text/plain' }));
        await navigator.clipboard.write([new ClipboardItem({ 'text/plain': blob })]);
        return;
      } catch (_) {
        // Engines that take only settled data land here; a failed load rethrows below.
      }
      await navigator.clipboard.writeText(await pending);
      return;
    }
    await navigator.clipboard.writeText(await text);
    return;
  }
  const area = document.createElement('textarea');
  area.value = await text;
  area.setAttribute('readonly', '');
  area.style.position = 'fixed';
  area.style.opacity = '0';
  document.body.appendChild(area);
  area.select();
  const ok = document.execCommand('copy');
  area.remove();
  if (!ok) throw new Error('the browser blocked clipboard access');
}

function ledgerHTML(text) {
  // Escaping never adds or removes a newline, so one pass over the whole text
  // matches escaping each line and avoids a regex call per line on big dumps.
  const lines = esc(text).split('\n');
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  // A real newline ends each line: Firefox copies the spans as one line despite
  // display:block, and WebKit drops the empty ones.
  const line = (j) => '<span class="ln" data-n="' + (j + 1) + '">' + lines[j] + '\n</span>';
  let html = '';
  if (lines.length <= LEDGER_CHUNK) {
    for (let j = 0; j < lines.length; j++) html += line(j);
    return html;
  }
  for (let i = 0; i < lines.length; i += LEDGER_CHUNK) {
    const end = Math.min(lines.length, i + LEDGER_CHUNK);
    html += '<div class="chunk" style="contain-intrinsic-size: auto ' + (end - i) * 20 + 'px">';
    for (let j = i; j < end; j++) html += line(j);
    html += '</div>';
  }
  return html;
}

/** Marks the combined sheet as showing the blank ruling rather than a dump. */
const BLANK_SHOWN = {};

/** Numbered, empty ruled lines for a sheet that has no dump yet. */
const BLANK_LEDGER = Array.from({ length: 80 }, (_, j) => '<span class="ln" data-n="' + (j + 1) + '" aria-hidden="true"></span>').join('');

const OPTION_ROWS = [
  { key: 'gitignore', label: 'gitignore', help: 'Honor .gitignore and .ignore files.', off: 'Needs the local mill (pulp ui).' },
  { key: 'hidden', label: 'Hidden files', help: 'Include dotfiles; .git stays out.' },
  { key: 'archives', label: 'Archives', help: 'Unpack nested zip and tar members.' },
  { key: 'notebook', label: 'Notebook outputs', help: 'Keep Jupyter cell outputs.' },
  { key: 'tree', label: 'Directory map', help: 'Put the tree at the top of the dump.' },
  { key: 'wrap', label: 'Wrap lines', help: 'View only; the dump is unchanged.' },
];

const DISCOVERY = ['gitignore', 'hidden', 'archives'];
const MAC = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent || '');
const PULP_HINT = MAC ? '⌘↩' : 'Ctrl ↩';

/** A touch screen with no hover: keyboard hints mean nothing there. */
function isTouch() {
  return !!(window.matchMedia && window.matchMedia('(hover: none) and (pointer: coarse)').matches);
}

let uidSeq = 0;

export function mountMill(root, adapter, options) {
  const opts = options || {};
  const demo = !!opts.demo;
  const uid = 'mill' + ++uidSeq;
  const caps = Object.assign({
    source: 'path',
    pickFiles: false,
    drop: false,
    gitignore: true,
    rerender: false,
    sample: false,
    progress: false,
  }, adapter.caps || {});

  const canDrop = caps.drop && !(window.matchMedia && window.matchMedia('(pointer: coarse)').matches);
  const stored = demo ? null : readStore(SETTINGS_KEY);
  const S = {
    busy: '',
    path: demo ? (opts.path || '') : (readStore(PATH_KEY) || ''),
    pathShown: '',
    sourceId: 0,
    sourceLabel: '',
    files: [],
    byId: new Map(),
    selected: new Set(),
    collapsed: new Set(),
    filter: '',
    settings: Object.assign({}, DEFAULTS, stored && typeof stored === 'object' ? stored : {}),
    scanKey: '',
    scanned: false,
    result: null,
    issues: new Map(),
    view: 'combined',
    active: '',
    preview: null,
    previewSeq: 0,
    tab: 'files',
    progress: null,
    alert: null,
    optsOpen: false,
    inspOpen: false,
    scanTried: '',
    renderTried: '',
    remember: true,
  };
  if (!caps.gitignore) S.settings.gitignore = false;
  let shownDump = null;
  let lastDock = '';
  let lastNote = '';
  let lastStats = '';
  let shownIssues = null;
  let flagDirs = new Set();
  let dockArmedAt = 0;
  let emptyMode = 'first';
  // Listeners outside the mill's root, removed by destroy().
  const unlisten = [];
  const listen = (target, type, fn, opts) => {
    target.addEventListener(type, fn, opts);
    unlisten.push(() => target.removeEventListener(type, fn, opts));
  };
  let chipOrder = '';
  let chipBodies = [];
  // Tree state; see the tree model section.
  let T = buildModel([]);
  let rows = [];
  let rowPos = [];
  let rowSize = [];
  let winStart = 0;
  let winEnd = 0;
  let rowH = 0;
  let listTop = 0;
  let padKey = '';
  let cursorKey = '';
  let cursorRow = 0;
  let parking = false;
  let pointerRow = null;
  let selVersion = 0;
  let selCheck = null;

  root.classList.add('mill');
  // The page mill owns the viewport; the landing's inert shot does not.
  root.classList.toggle('is-page', !demo);
  root.dataset.surface = adapter.surface || 'local';
  root.innerHTML = shellHTML();
  const $ = (name) => root.querySelector('[data-el="' + name + '"]');
  const el = {
    path: $('path'),
    srcMeta: $('srcmeta'),
    srcLabel: $('srclabel'),
    alert: $('alert'),
    tabs: $('tabs'),
    filesHead: $('filecount'),
    filesEmpty: $('filesempty'),
    filesLoading: $('filesloading'),
    filesBody: $('filesbody'),
    filter: $('filter'),
    chips: $('chips'),
    tree: $('tree'),
    rows: $('rows'),
    progress: $('progress'),
    views: $('views'),
    issueCount: $('issuecount'),
    outKeys: $('outkeys'),
    outCopy: root.querySelector('[data-el="outkeys"] [data-act="copy"]'),
    outDownload: root.querySelector('[data-el="outkeys"] [data-act="download"]'),
    outTree: root.querySelector('[data-el="outkeys"] [data-act="tree"]'),
    outNote: $('outnote'),
    dump: $('dump'),
    watermark: $('watermark'),
    prevHead: $('prevhead'),
    prevNote: $('prevnote'),
    prevBody: $('prevbody'),
    issues: $('issues'),
    insp: $('insp'),
    sumText: $('sumtext'),
    contentHint: $('contenthint'),
    opts: $('opts'),
    optsToggle: $('optstoggle'),
    pulp: $('pulpkey'),
    pulpHint: root.querySelector('[data-el="pulpkey"] .p-kbdhint'),
    cancel: $('cancelkey'),
    status: $('status'),
    why: $('why'),
    stats: $('stats'),
    dock: $('dock'),
    footSel: $('footsel'),
    toasts: $('toasts'),
    drop: $('drop'),
  };
  S.pathShown = S.path;
  showPathEnd(S.path);
  syncSettingsInputs();
  applyWrap();
  renderAll();

  /* ---------- markup ---------- */

  function shellHTML() {
    const local = caps.source === 'path';
    const brand = adapter.homeHref
      ? '<a class="mill-brand" href="' + esc(adapter.homeHref) + '"><span class="p-mark" aria-hidden="true"></span><span class="p-word">pulp</span></a>'
      : '<span class="mill-brand"><span class="p-mark" aria-hidden="true"></span><span class="p-word">pulp</span></span>';
    const source = local
      ? '<button type="button" class="p-key" data-act="browse">' + icon('folder') + '<span class="mill-key-text">Browse</span></button>' +
        '<label class="p-field mill-path">' + icon('folder') +
        '<input data-el="path" name="path" type="text" placeholder="~/path/to/folder" spellcheck="false" autocapitalize="off" autocorrect="off" autocomplete="off" enterkeyhint="go" aria-label="Folder path">' +
        '<span class="p-meta" data-el="srcmeta"></span><kbd class="p-kbd mill-path-kbd" aria-hidden="true">↩</kbd></label>' +
        '<button type="button" class="p-key" data-act="scan">' + icon('search') + '<span class="mill-scan-label">Scan</span></button>'
      : '<button type="button" class="p-key" data-act="browse">' + icon('folder') + '<span class="mill-key-text">Choose folder</span></button>' +
        (caps.pickFiles ? '<button type="button" class="p-key" data-act="pick-files">' + icon('file') + '<span class="mill-key-text">Choose files</span></button>' : '') +
        '<div class="mill-grant">' + icon('files') + '<span data-el="srclabel"></span></div>';
    const optRows = OPTION_ROWS.map((o) => {
      const off = o.key === 'gitignore' && !caps.gitignore;
      const id = uid + '-opt-' + o.key;
      return '<label class="mill-opt' + (off ? ' is-disabled' : '') + '"><input type="checkbox" class="p-check" data-opt="' + o.key + '"' +
        ' aria-labelledby="' + id + '" aria-describedby="' + id + '-help"' + (off ? ' disabled' : '') + '><span><b id="' + id + '">' + esc(o.label) +
        '</b><small id="' + id + '-help">' + esc(off ? o.off : o.help) + '</small></span></label>';
    }).join('');
    const seg = (name, items) => '<div class="p-seg" role="radiogroup" aria-labelledby="' + uid + '-' + name + '">' + items.map((it) =>
      '<label><input type="radio" name="' + name + '-' + uid + '" data-setting="' + name + '" value="' + it[0] + '"><span>' + it[1] + '</span></label>').join('') + '</div>';
    const box = (tag) => (demo ? 'div' : tag);
    return (
      '<' + box('header') + ' class="mill-top">' + brand + (demo ? '' : '<h1 class="sr-only">pulp mill</h1>') +
      '<span class="p-label">mill</span><span class="p-grow"></span>' +
      '<span class="mill-priv p-label">' + icon('lock') + '<span class="mill-priv-text">' + esc(adapter.privacy || 'nothing leaves this machine') + '</span></span>' +
      '<a class="p-key is-sq" href="' + REPO_URL + '" target="_blank" rel="noopener noreferrer" aria-label="pulp source on GitHub" title="pulp source on GitHub">' + icon('github') + '</a></' + box('header') + '>' +
      '<' + box('main') + ' class="mill-body">' +
      '<' + box('section') + ' class="mill-source' + (local ? ' is-path' : '') + '"' + (demo ? '' : ' aria-label="Folder"') + '>' + source + '</' + box('section') + '>' +
      '<div class="mill-alert" data-el="alert" hidden></div>' +
      '<div class="mill-tabs" data-el="tabs" role="tablist" aria-label="Mill panes">' +
      '<button type="button" role="tab" id="' + uid + '-tab-files" aria-controls="' + uid + '-files" data-tab="files" aria-selected="true">Files <span class="mill-tabcount" data-el="tabcount"></span></button>' +
      '<button type="button" role="tab" id="' + uid + '-tab-output" aria-controls="' + uid + '-out" data-tab="output" aria-selected="false"><span class="mill-tabdot" data-el="tabdot" hidden></span>Output</button></div>' +
      '<div class="mill-grid">' +
      '<section class="p-pane mill-files" id="' + uid + '-files" aria-label="Files">' +
      '<div class="mill-pane-h"><span class="p-label">Files</span><span class="p-meta" data-el="filecount"></span><span class="p-grow"></span>' +
      '<button type="button" class="p-key is-quiet is-sm" data-act="all" title="Tick every file that matches the filter">All</button>' +
      '<button type="button" class="p-key is-quiet is-sm" data-act="none" title="Untick every file that matches the filter">None</button></div>' +
      '<div class="mill-empty" data-el="filesempty">' + emptyHTML() + '</div>' +
      // Focusable, so keyboard focus can wait here while a sample or a scan loads.
      '<div class="mill-loading" data-el="filesloading" tabindex="-1" hidden><span class="p-spin"></span><span>scanning</span></div>' +
      '<div data-el="filesbody" hidden style="display:flex;flex-direction:column;min-height:0;flex:1 1 auto">' +
      '<div class="mill-filter"><label class="p-field">' + icon('search') +
      '<input data-el="filter" name="filter" type="search" placeholder="filter" spellcheck="false" autocapitalize="off" autocorrect="off" autocomplete="off" enterkeyhint="search" aria-label="Filter files" aria-keyshortcuts="/">' +
      '<kbd class="p-kbd">/</kbd></label><span class="mill-filter-keys">' +
      '<button type="button" class="p-key is-quiet is-sm" data-act="all" title="Tick every file that matches the filter">All</button>' +
      '<button type="button" class="p-key is-quiet is-sm" data-act="none" title="Untick every file that matches the filter">None</button></span></div>' +
      '<div class="mill-chips" data-el="chips" role="group" aria-label="Tick by language"></div>' +
      '<div class="mill-tree" data-el="tree"><div class="mill-rows" data-el="rows" role="tree" aria-label="Files"></div></div></div></section>' +
      // Settings come before the output in the DOM: the order of the task, and the
      // strip's visual order below 1180px.
      '<aside class="p-pane mill-insp" data-el="insp" aria-label="Settings">' +
      '<div class="mill-pane-h"><span class="p-label">Settings</span></div>' +
      '<div class="mill-insp-body">' +
      '<div class="mill-sum"><span class="p-meta" data-el="sumtext"></span>' +
      '<button type="button" class="p-key is-sm" data-act="insp" aria-expanded="false">' + icon('sliders') + 'Settings</button></div>' +
      // Each group's label sits above its keys in the wide inspector and beside them in the strip.
      // The groups scroll on their own in the wide inspector, so Pulp and the stats stay in view.
      '<div class="mill-insp-sets">' +
      '<div class="mill-set"><span class="p-label" id="' + uid + '-content">Content</span>' + seg('content', [['readable', 'Readable'], ['source', 'Source']]) +
      '<p class="mill-hint" data-el="contenthint" hidden>HTML, XML, and JSON stay as decoded source; PDFs and Office files are still turned into text.</p></div>' +
      '<div class="mill-set"><span class="p-label" id="' + uid + '-format">Format</span>' + seg('format', [['txt', 'Txt'], ['md', 'Md'], ['xml', 'Xml']]) + '</div>' +
      '<div class="mill-set mill-set-opts" role="group" aria-labelledby="' + uid + '-options"><span class="p-label" id="' + uid + '-options">Options</span>' +
      '<button type="button" class="p-key is-sm mill-opts-toggle" data-el="optstoggle" data-act="opts" aria-expanded="false">' + icon('sliders') + 'Options<span class="p-count" data-el="optscount"></span></button>' +
      '<div class="mill-opts" data-el="opts">' + optRows + '</div></div></div>' +
      '<div class="mill-run">' +
      '<button type="button" class="p-key is-primary is-lg is-block" data-act="pulp" data-el="pulpkey" title="Pulp (Ctrl+Enter or Cmd+Enter)" aria-keyshortcuts="Meta+Enter Control+Enter">' + icon('press') +
        '<span data-el="pulptext">Pulp</span><span class="p-kbdhint" aria-hidden="true">' + PULP_HINT + '</span></button>' +
      '<button type="button" class="p-key is-block" data-act="cancel" data-el="cancelkey" aria-keyshortcuts="Escape" hidden>' + icon('x') +
        'Cancel<span class="p-kbdhint" aria-hidden="true">esc</span></button>' +
      // Only the status is live; the progress beside it would announce several times a second.
      '<div class="mill-statusline"><span class="p-status" data-el="status" data-tone="idle" aria-live="polite">Not generated</span>' +
      '<span class="p-meta mill-statmeta" data-el="statmeta"></span></div>' +
      '<p class="mill-why" data-el="why"></p><dl class="mill-stats" data-el="stats" hidden></dl></div></div></aside>' +
      '<section class="p-pane mill-out" id="' + uid + '-out" aria-label="Output">' +
      '<div class="p-progress" data-el="progress" hidden></div>' +
      '<div class="p-tabs mill-views"><div class="mill-viewtabs" data-el="views" role="tablist" aria-label="Output view">' +
      viewTab('combined', 'Combined') + viewTab('preview', 'Preview') +
      viewTab('issues', 'Issues<span class="p-badge" data-el="issuecount" hidden></span>') +
      '</div><span class="p-grow"></span><div class="mill-outkeys" data-el="outkeys">' +
      '<button type="button" class="p-key" data-act="copy" aria-disabled="true">' + icon('copy') + 'Copy</button>' +
      '<button type="button" class="p-key" data-act="download" aria-disabled="true">' + icon('download') + 'Download</button>' +
      '<button type="button" class="p-key is-quiet" data-act="tree" aria-disabled="true" title="Copy only the directory map">' + icon('tree') + 'Tree</button></div></div>' +
      '<div class="mill-outnote" data-el="outnote" hidden></div>' +
      '<div class="p-sheet mill-sheet" ' + panelAttrs('combined') + '><div class="p-ledger" data-el="dump" tabindex="0" role="region" aria-label="Combined dump"></div>' +
      '<div class="mill-watermark" data-el="watermark" aria-hidden="true"></div></div>' +
      '<div class="p-sheet mill-sheet" ' + panelAttrs('preview') + ' hidden><div class="mill-prevhead" data-el="prevhead"></div>' +
      '<div class="mill-prevnote" data-el="prevnote" hidden></div>' +
      '<div class="p-ledger" data-el="prevbody" tabindex="0" role="region" aria-label="File preview"></div></div>' +
      '<div class="mill-issues" ' + panelAttrs('issues') + ' data-el="issues" hidden></div></section>' +
      '</div>' +
      '<' + box('section') + ' class="mill-dock" data-el="dock"' + (demo ? '' : ' aria-label="Actions"') + '></' + box('section') + '>' +
      '</' + box('main') + '>' +
      '<' + box('footer') + ' class="mill-foot"><span data-el="footsel"></span><span class="p-grow"></span><span>' + esc(adapter.footer || '') + '</span></' + box('footer') + '>' +
      '<div class="p-toasts" data-el="toasts" role="status" aria-live="polite"></div>' +
      (caps.drop ? '<div class="mill-drop" data-el="drop" hidden><div>' + icon('tray') + '<strong>Drop a folder or files</strong><span class="p-meta">they stay in this tab</span></div></div>' : '')
    );
  }

  function viewTab(view, label) {
    return '<button type="button" class="p-tab" role="tab" data-view="' + view + '" id="' + uid + '-view-' + view + '" aria-controls="' +
      uid + '-panel-' + view + '">' + label + '</button>';
  }

  function panelAttrs(view) {
    return 'data-panel="' + view + '" role="tabpanel" id="' + uid + '-panel-' + view + '" aria-labelledby="' + uid + '-view-' + view + '"';
  }

  function emptyHTML() {
    const local = caps.source === 'path';
    const first = local
      ? ['Choose a folder', 'Browse opens your file manager, or paste a path above.']
      : ['Choose a folder', canDrop ? 'Or choose files, or drop a folder anywhere on this page.' : 'Or choose individual files.'];
    return '<ol class="mill-steps">' +
      '<li><b>01</b><div><strong>' + first[0] + '</strong><span>' + first[1] + '</span></div></li>' +
      '<li><b>02</b><div><strong>Tick what goes in</strong><span>Lockfiles, images, and virtualenvs start unticked.</span></div></li>' +
      '<li><b>03</b><div><strong>Pulp</strong><span>One dump as XML, Markdown, or plain text. Copy it or download it.</span></div></li></ol>' +
      '<div class="mill-empty-keys"><button type="button" class="p-key is-primary" data-act="browse">' + icon('folder') + (local ? 'Browse' : 'Choose folder') + '</button>' +
      (caps.sample ? '<button type="button" class="p-key" data-act="sample">' + icon('flask') + 'Try a sample</button>' : '') + '</div>' +
      '<p class="mill-fine">' + icon('lock') + esc(adapter.privacyLong || 'Files never leave this machine.') + '</p>';
  }

  /** Put `text` in the path field, scrolled to its end, where the folder name is. */
  /** The files pane after a scan that found nothing to pulp. */
  function noFilesHTML() {
    const local = caps.source === 'path';
    const widen = caps.gitignore
      ? 'Tick Hidden files, or turn off gitignore, under Options to include more.'
      : 'Tick Hidden files under Options to include dotfiles.';
    return '<div class="mill-nofiles"><strong>Nothing to pulp here.</strong>' +
      '<span>Everything in this folder is excluded (build output, virtualenvs, secrets), hidden, or empty. ' + widen + '</span></div>' +
      '<div class="mill-empty-keys"><button type="button" class="p-key is-primary" data-act="browse">' + icon('folder') +
      (local ? 'Browse' : 'Choose another folder') + '</button></div>';
  }

  function showPathEnd(text) {
    if (!el.path) return;
    el.path.value = text;
    scrollPathEnd();
  }

  /** Browsers scroll a field back to its start on blur; the folder name is the useful end. */
  function scrollPathEnd() {
    if (!el.path) return;
    window.requestAnimationFrame(() => {
      el.path.scrollLeft = el.path.scrollWidth;
    });
  }

  /* ---------- keys ---------- */

  /**
   * Take the folder from the path field. Text still showing a source's display
   * name (the sample's) keeps that source; anything typed becomes the new path.
   */
  function pathFromField() {
    if (!el.path) return;
    const typed = el.path.value.trim();
    if (S.path && typed === S.pathShown) return;
    S.path = typed;
    S.pathShown = typed;
    S.remember = true;
  }

  function discoveryKey() {
    const s = S.settings;
    return JSON.stringify([caps.gitignore && s.gitignore, s.hidden, s.archives]);
  }

  // The keys cover settings only; the ticked set is compared on its own (sameSelection)
  // so a large selection is never sorted and serialized on every render.
  function extractKey() {
    const s = S.settings;
    return JSON.stringify([S.sourceId, s.content, s.notebook, discoveryKey()]);
  }

  function packKey() {
    return JSON.stringify([extractKey(), S.settings.format, S.settings.tree]);
  }

  /** Whether the ticked set equals `picked`, cached until the next tick. */
  function sameSelection(picked) {
    if (!picked || picked.size !== S.selected.size) return false;
    if (selCheck && selCheck.version === selVersion && selCheck.picked === picked) return selCheck.same;
    let same = true;
    for (const id of picked) {
      if (!S.selected.has(id)) {
        same = false;
        break;
      }
    }
    selCheck = { version: selVersion, picked, same };
    return same;
  }

  function isStale() {
    return !!S.result && (packKey() !== S.result.key || !sameSelection(S.result.picked));
  }

  function primary() {
    if (!S.files.length) return 'source';
    if (S.result && !isStale()) return 'copy';
    return 'pulp';
  }

  function selectedBytes() {
    return T.selBytes;
  }

  function packRequest() {
    const s = S.settings;
    return {
      path: S.path,
      selected: [...S.selected],
      format: s.format,
      source: s.content === 'source',
      tree: s.tree,
      notebook: s.notebook,
      archives: s.archives,
      hidden: s.hidden,
      gitignore: caps.gitignore ? s.gitignore : false,
      resultId: S.result ? S.result.resultId : '',
    };
  }

  /* ---------- tree model ---------- */

  // Built once per scan; files are addressed by their index in S.files. Each folder
  // counts, under the current filter, the files shown below it (vis), those that
  // can be ticked (total), and those ticked (on), so a tick touches O(depth)
  // counters instead of recounting the scan.

  function folder(name, path, parent) {
    return {
      name, path, parent, depth: parent ? parent.depth + 1 : -1,
      kids: [], files: [], sorted: false, count: 0, own: 0, vis: 0, total: 0, on: 0,
    };
  }

  function buildModel(files) {
    const n = files.length;
    const top = folder('', '', null);
    const dirs = new Map();
    const order = [];
    const ids = new Array(n);
    const index = new Map();
    const parent = new Array(n);
    const lang = new Int32Array(n);
    const langs = [];
    const langIds = new Map();
    const byLang = [];
    const size = new Float64Array(n);
    const ok = new Uint8Array(n);
    const on = new Uint8Array(n);
    let bytes = 0;
    let selBytes = 0;
    const walk = (rel) => {
      const parts = rel.split('/').filter(Boolean);
      let node = top;
      for (let k = 0; k < parts.length - 1; k++) {
        const path = node.path ? node.path + '/' + parts[k] : parts[k];
        let next = dirs.get(path);
        if (!next) {
          next = folder(parts[k], path, node);
          dirs.set(path, next);
          node.kids.push(next);
          order.push(next);
        }
        node = next;
      }
      return node;
    };
    for (let i = 0; i < n; i++) {
      const f = files[i];
      const rel = f.relative;
      // A clean path's folder is its prefix, so most files skip the split and walk.
      const cut = rel.lastIndexOf('/');
      const clean = cut > 0 && cut < rel.length - 1 && rel[0] !== '/' && !rel.includes('//');
      const node = cut < 0 ? top : (clean && dirs.get(rel.slice(0, cut))) || walk(rel);
      node.files.push(i);
      node.count++;
      parent[i] = node;
      const id = fileId(f);
      ids[i] = id;
      index.set(id, i);
      const name = langOf(f);
      let l = langIds.get(name);
      if (l === undefined) {
        l = langs.length;
        langIds.set(name, l);
        langs.push(name);
        byLang.push([]);
      }
      lang[i] = l;
      byLang[l].push(i);
      size[i] = Number(f.size) || 0;
      bytes += size[i];
      ok[i] = f.oversized ? 0 : 1;
      if (S.selected.has(id)) {
        on[i] = 1;
        selBytes += size[i];
      }
    }
    for (let k = order.length - 1; k >= 0; k--) order[k].parent.count += order[k].count;
    return {
      files, n, top, dirs, order, ids, index, parent, lang, langs, langIds, byLang, size, ok, on, bytes, selBytes,
      langOn: new Int32Array(langs.length), langTotal: new Int32Array(langs.length),
      vis: null, shown: n, low: null, cache: new Map(),
    };
  }

  function isShown(i) {
    return !T.vis || T.vis[i] === 1;
  }

  /** Files that match query q as a 0/1 array, or null for no query. Cached per query. */
  function matchesFor(q) {
    if (!q) return null;
    const hit = T.cache.get(q);
    if (hit) {
      T.cache.delete(q);
      T.cache.set(q, hit);
      return hit;
    }
    if (!T.low) T.low = T.files.map((f) => f.relative.toLowerCase());
    const langHit = T.langs.map((l) => l.toLowerCase().includes(q));
    // A query containing an earlier one can only narrow its matches.
    let base = null;
    let baseLen = -1;
    for (const [p, m] of T.cache) {
      if (p.length > baseLen && q.includes(p)) {
        base = m;
        baseLen = p.length;
      }
    }
    const out = new Uint8Array(T.n);
    const low = T.low;
    for (let i = 0; i < T.n; i++) {
      if (base && !base[i]) continue;
      if (langHit[T.lang[i]] || low[i].includes(q)) out[i] = 1;
    }
    T.cache.set(q, out);
    if (T.cache.size > 16) T.cache.delete(T.cache.keys().next().value);
    return out;
  }

  /** Apply S.filter and recount every folder and language for it. */
  function applyFilter() {
    T.vis = matchesFor(S.filter);
    const { top, order, n, vis, ok, on, lang, parent, langOn, langTotal } = T;
    for (const d of order) d.own = d.vis = d.total = d.on = 0;
    top.own = top.vis = top.total = top.on = 0;
    langOn.fill(0);
    langTotal.fill(0);
    let shown = 0;
    for (let i = 0; i < n; i++) {
      if (vis && !vis[i]) continue;
      shown++;
      const p = parent[i];
      p.own++;
      if (!ok[i]) continue;
      p.total++;
      langTotal[lang[i]]++;
      if (on[i]) {
        p.on++;
        langOn[lang[i]]++;
      }
    }
    for (const d of order) d.vis = d.own;
    top.vis = top.own;
    for (let k = order.length - 1; k >= 0; k--) {
      const d = order[k];
      const up = d.parent;
      up.vis += d.vis;
      up.total += d.total;
      up.on += d.on;
    }
    T.shown = shown;
  }

  /** Tick or untick file i, keeping S.selected and every counter in step. */
  function tick(i, value) {
    if ((T.on[i] === 1) === value) return;
    T.on[i] = value ? 1 : 0;
    if (value) S.selected.add(T.ids[i]);
    else S.selected.delete(T.ids[i]);
    selVersion++;
    const d = value ? 1 : -1;
    T.selBytes += d * T.size[i];
    if (T.ok[i] && isShown(i)) {
      for (let p = T.parent[i]; p; p = p.parent) p.on += d;
      T.langOn[T.lang[i]] += d;
    }
  }

  /** Sort a folder's children the first time it is shown. */
  function sortFolder(node) {
    if (node.sorted) return;
    const files = T.files;
    node.kids.sort((a, b) => a.name.localeCompare(b.name));
    node.files.sort((a, b) => files[a].relative.localeCompare(files[b].relative));
    node.sorted = true;
  }

  function isCollapsed(path) {
    return !S.filter && S.collapsed.has(path);
  }

  /** Flatten the shown tree into rows: a folder node or a file index each. */
  function buildRows() {
    const out = [];
    const pos = [];
    const size = [];
    const vis = T.vis;
    const walk = (node) => {
      sortFolder(node);
      let set = node.own;
      for (const d of node.kids) if (d.vis) set++;
      let k = 0;
      for (const d of node.kids) {
        if (!d.vis) continue;
        out.push(d);
        pos.push(++k);
        size.push(set);
        if (!isCollapsed(d.path)) walk(d);
      }
      for (const i of node.files) {
        if (vis && !vis[i]) continue;
        out.push(i);
        pos.push(++k);
        size.push(set);
      }
    };
    walk(T.top);
    rows = out;
    rowPos = pos;
    rowSize = size;
  }

  function depthOf(r) {
    const ref = rows[r];
    return typeof ref === 'number' ? T.parent[ref].depth + 1 : ref.depth;
  }

  /** The row index of r's folder, or -1 at the top level. */
  function parentRow(r) {
    const ref = rows[r];
    const up = typeof ref === 'number' ? T.parent[ref] : ref.parent;
    return up && up !== T.top ? rows.lastIndexOf(up, r) : -1;
  }

  /* ---------- render ---------- */

  function renderAll() {
    renderSource();
    renderFiles();
    renderSettingsState();
    renderOutput();
    renderRunState();
    renderAlert();
  }

  /**
   * Mark a key usable or not. Keys stay focusable either way (aria-disabled, not
   * disabled), so keyboard focus survives a scan or a pack; the click handler
   * ignores unavailable keys.
   */
  function setAvailable(key, on) {
    if (!key) return;
    if (on) key.removeAttribute('aria-disabled');
    else key.setAttribute('aria-disabled', 'true');
  }

  function renderSource() {
    const busy = !!S.busy;
    root.querySelectorAll('.mill-source [data-act], .mill-empty [data-act]').forEach((b) => setAvailable(b, !busy));
    // The empty state (or the phone dock) carries the one primary Browse key.
    if (el.srcMeta) {
      el.srcMeta.textContent = S.scanned && S.files.length ? plural(S.files.length, 'file') + ' · ' + human(T.bytes) : '';
    }
    if (el.srcLabel) {
      el.srcLabel.textContent = S.sourceLabel ||
        (canDrop ? 'No files yet. Drop a folder anywhere on this page.' : 'No files yet.');
    }
  }

  function renderFiles() {
    renderFilesChrome();
    renderChips();
    renderTree();
  }

  function renderFilesChrome() {
    const has = S.files.length > 0;
    root.classList.toggle('has-files', has);
    const mode = S.scanned && !has ? 'none' : 'first';
    if (mode !== emptyMode) {
      const hadFocus = el.filesEmpty.contains(document.activeElement);
      el.filesEmpty.innerHTML = mode === 'none' ? noFilesHTML() : emptyHTML();
      emptyMode = mode;
      if (hadFocus) rescueFocus(true, 'tree');
    }
    el.filesEmpty.hidden = has || S.busy === 'scan' || S.busy === 'sample';
    el.filesLoading.hidden = !(S.busy === 'scan' || S.busy === 'sample') || has;
    el.filesBody.hidden = !has;
    root.querySelectorAll('.mill-files [data-act="all"], .mill-files [data-act="none"]').forEach((b) => {
      b.hidden = !has;
    });
    renderCounts();
  }

  function renderCounts() {
    const n = S.selected.size;
    el.filesHead.textContent = S.files.length ? n + ' / ' + S.files.length : '';
    const tabCount = root.querySelector('[data-el="tabcount"]');
    if (tabCount) tabCount.textContent = S.files.length ? n + '/' + S.files.length : '';
    el.footSel.innerHTML = S.files.length
      ? '<b>' + n + '</b> of ' + S.files.length + ' ticked · ' + human(selectedBytes()) +
        (S.filter ? ' · ' + T.shown + ' match the filter' : '')
      : (S.busy === 'scan' ? 'scanning…' : S.scanned ? 'nothing to pulp in this folder' : 'no folder');
  }

  /** Where focus sits among the chips or in the tree, so a render can put it back. */
  function focusKey() {
    const a = document.activeElement;
    if (!a || !root.contains(a)) return null;
    if (a.dataset.act === 'chip') return { sel: '[data-act="chip"][data-lang="' + CSS.escape(a.dataset.lang) + '"]' };
    if (el.rows.contains(a)) return { tree: true, act: a === el.rows ? '' : a.dataset.act || '' };
    return null;
  }

  function restoreFocus(key) {
    if (!key) return;
    if (key.tree) {
      focusCursor(key.act);
      return;
    }
    const host = root.querySelector(key.sel);
    if (host && host !== document.activeElement) host.focus({ preventScroll: true });
  }

  function chipParts(l) {
    const on = T.langOn[l];
    const total = T.langTotal[l];
    const state = on === 0 ? 'off' : on === total ? 'on' : 'mix';
    return { state, body: (state === 'on' ? icon('check') : '') + esc(T.langs[l]) + '<em>' + on + '/' + total + '</em>' };
  }

  function chipPressed(state) {
    return state === 'on' ? 'true' : state === 'mix' ? 'mixed' : 'false';
  }

  function renderChips() {
    const list = [];
    for (let l = 0; l < T.langs.length; l++) if (T.langTotal[l]) list.push(l);
    list.sort((a, b) => T.langTotal[b] - T.langTotal[a] || T.langs[a].localeCompare(T.langs[b]));
    const order = list.map((l) => T.langs[l]).join('\n');
    const parts = list.map(chipParts);
    if (order === chipOrder) {
      // Same chips in the same order: patch the ones whose counts moved, so a
      // focused chip stays the same element.
      parts.forEach((p, j) => {
        if (chipBodies[j] === p.body) return;
        const b = el.chips.children[j];
        b.dataset.state = p.state;
        b.setAttribute('aria-pressed', chipPressed(p.state));
        b.innerHTML = p.body;
        chipBodies[j] = p.body;
      });
      return;
    }
    const focus = focusKey();
    el.chips.innerHTML = list.map((l, j) => {
      const k = esc(T.langs[l]);
      return '<button type="button" class="p-chip" data-act="chip" data-lang="' + k + '" data-state="' + parts[j].state + '" aria-pressed="' +
        chipPressed(parts[j].state) + '" title="Tick or untick every ' + k + ' file">' + parts[j].body + '</button>';
    }).join('');
    chipOrder = order;
    chipBodies = parts.map((p) => p.body);
    restoreFocus(focus);
  }

  /* ---------- tree rows ----------
     Only the rows near the viewport are in the DOM (all of them up to FULL_TREE);
     padding on the list stands in for the rest. One control in the tree is
     tabbable: the cursor row's checkbox, or its name when the box is disabled. */

  function renderTree() {
    buildRows();
    placeCursor();
    el.rows.hidden = !rows.length;
    // The message exists only while nothing matches, as it always has.
    let none = el.tree.querySelector('.mill-none');
    if (!rows.length && S.files.length) {
      if (!none) {
        none = document.createElement('p');
        none.className = 'mill-none';
        el.tree.appendChild(none);
      }
      none.textContent = 'No files match “' + S.filter + '”.';
    } else if (none) {
      none.remove();
    }
    paintRows(true);
  }

  /** Row height in px: measured from a rendered row, else the --row token. */
  function rowHeight() {
    if (rowH) return rowH;
    const first = el.rows.firstElementChild;
    const h = first ? first.offsetHeight : 0;
    if (h > 0) {
      rowH = h;
      listTop = parseFloat(getComputedStyle(el.tree).paddingTop) || 0;
      return rowH;
    }
    return parseFloat(getComputedStyle(el.tree).getPropertyValue('--row')) || 26;
  }

  /**
   * Render the rows around the tree's viewport. `force` redraws every rendered
   * row (the rows or their markup changed); otherwise the window only shifts,
   * keeping the rows that stay.
   */
  function paintRows(force) {
    const total = rows.length;
    const measured = !!rowH;
    const h = rowHeight();
    let start = 0;
    let end = total;
    if (total > FULL_TREE) {
      const top = el.tree.scrollTop - listTop;
      const first = Math.min(total - 1, Math.max(0, Math.floor(top / h)));
      const last = Math.min(total, Math.max(first + 1, Math.ceil((top + (el.tree.clientHeight || 800)) / h)));
      const spare = OVERSCAN >> 2;
      if (force || winStart > Math.max(0, first - spare) || winEnd < Math.min(total, last + spare)) {
        start = Math.max(0, first - OVERSCAN);
        end = Math.min(total, last + OVERSCAN);
      } else {
        start = winStart;
        end = winEnd;
      }
    }
    if (force || start !== winStart || end !== winEnd) {
      const focus = focusKey();
      if (force || end <= winStart || start >= winEnd) {
        el.rows.innerHTML = rowsHTML(start, end);
      } else {
        for (let k = winStart; k < start; k++) el.rows.firstElementChild.remove();
        for (let k = end; k < winEnd; k++) el.rows.lastElementChild.remove();
        if (start < winStart) el.rows.insertAdjacentHTML('afterbegin', rowsHTML(start, winStart));
        if (end > winEnd) el.rows.insertAdjacentHTML('beforeend', rowsHTML(winEnd, end));
      }
      winStart = start;
      winEnd = end;
      syncRows();
      restoreFocus(focus);
      setTabStop();
    }
    // The first rendered row may measure differently from the token (touch rows
    // carry a border); place the window again with the real height.
    if (!measured && rowH && rowH !== h && total > FULL_TREE) {
      paintRows(false);
      return;
    }
    const height = rowHeight();
    const above = start * height + 'px';
    const below = (total - end) * height + 'px';
    if (above + below !== padKey) {
      padKey = above + below;
      el.rows.style.paddingTop = above;
      el.rows.style.paddingBottom = below;
    }
  }

  function rowsHTML(a, b) {
    let html = '';
    for (let r = a; r < b; r++) html += typeof rows[r] === 'number' ? fileRow(rows[r], r) : dirRow(rows[r], r);
    return html;
  }

  function treeAttrs(r, depth, label) {
    return ' role="treeitem" aria-level="' + (depth + 1) + '" aria-setsize="' + rowSize[r] + '" aria-posinset="' + rowPos[r] +
      '" aria-label="' + esc(label) + '"';
  }

  function dirRow(d, r) {
    const collapsed = isCollapsed(d.path);
    const off = d.total === 0;
    const tab = r === cursorRow;
    const flagged = collapsed && flagDirs.has(d.path);
    return '<div class="mill-row is-dir" style="--d:' + d.depth + '" data-dir="' + esc(d.path) + '"' +
      treeAttrs(r, d.depth, d.name + '/' + (flagged ? ', holds flagged files' : '')) +
      ' aria-expanded="' + !collapsed + '">' +
      '<button type="button" class="mill-twist" data-act="twist" tabindex="-1" aria-expanded="' + !collapsed + '" aria-label="' + (collapsed ? 'Expand ' : 'Collapse ') + esc(d.path) + '/">' + icon('chev') + '</button>' +
      '<label class="mill-check"><input type="checkbox" class="p-check" data-act="dircheck" tabindex="' + (tab && !off ? 0 : -1) + '" aria-label="Include ' + esc(d.path) + '/"' +
      (d.total > 0 && d.on === d.total ? ' checked' : '') + (off ? ' disabled' : '') + '></label>' + icon('folder') +
      '<button type="button" class="mill-name" data-act="twist" tabindex="' + (tab && off ? 0 : -1) + '">' + esc(d.name) + '/</button>' +
      (flagged ? '<span title="Holds flagged files">' + icon('alert', 'mill-flag') + '</span>' : '') +
      '<span class="mill-n">' + d.count + '</span></div>';
  }

  function fileRow(i, r) {
    const f = T.files[i];
    const id = T.ids[i];
    const name = f.relative.split('/').pop() || f.relative;
    const issue = S.issues.get(id);
    const on = T.on[i] === 1;
    const tab = r === cursorRow;
    const depth = T.parent[i].depth + 1;
    const cls = 'mill-row' + (on ? '' : ' is-off') + (S.active === id ? ' is-active' : '') + (issue ? ' is-flag' : '');
    return '<div class="' + cls + '" style="--d:' + depth + '" data-id="' + esc(id) + '"' + treeAttrs(r, depth, name) + '>' +
      '<span class="mill-twist" aria-hidden="true"></span>' +
      '<label class="mill-check"><input type="checkbox" class="p-check" data-act="check" tabindex="' + (tab && !f.oversized ? 0 : -1) + '" aria-label="Include ' + esc(f.relative) + '"' +
      (on ? ' checked' : '') + (f.oversized ? ' disabled title="Over the size cap"' : '') + '></label>' + icon('file') +
      '<button type="button" class="mill-name" data-act="preview" tabindex="' + (tab && f.oversized ? 0 : -1) + '" title="Preview ' + esc(f.relative) + '" aria-label="Preview ' + esc(f.relative) +
      (issue ? ', flagged: ' + esc(issue.status.replace(/_/g, ' ')) : '') + '">' + esc(name) + '</button>' +
      (issue ? '<span title="' + esc(issue.status) + ': select the file to see why">' + icon('alert', 'mill-flag') + '</span>' : '') +
      (f.oversized ? '<span class="p-tag">over cap</span>' : '') +
      '<span class="mill-lang">' + esc(langOf(f)) + '</span><span class="mill-size">' + human(f.size) + '</span></div>';
  }

  /** Bring the rendered rows' tick state and classes up to date in place. */
  function syncRows() {
    const list = el.rows.children;
    for (let k = 0; k < list.length; k++) {
      const row = list[k];
      const ref = rows[winStart + k];
      const box = row.querySelector('.p-check');
      if (typeof ref === 'number') {
        const id = T.ids[ref];
        const on = T.on[ref] === 1;
        if (box.checked !== on) box.checked = on;
        row.classList.toggle('is-off', !on);
        row.classList.toggle('is-active', S.active === id);
        row.classList.toggle('is-flag', S.issues.has(id));
      } else {
        const checked = ref.total > 0 && ref.on === ref.total;
        const mixed = ref.on > 0 && ref.on < ref.total;
        if (box.checked !== checked) box.checked = checked;
        if (box.indeterminate !== mixed) box.indeterminate = mixed;
        if (box.disabled !== (ref.total === 0)) box.disabled = ref.total === 0;
      }
    }
  }

  function rowEl(r) {
    return r >= winStart && r < winEnd ? el.rows.children[r - winStart] : null;
  }

  function rowIndex(row) {
    const k = Array.prototype.indexOf.call(el.rows.children, row);
    return k < 0 ? -1 : winStart + k;
  }

  /** The control that holds a row's tab stop: its checkbox, or its name when the box is disabled. */
  function rowTarget(row) {
    const box = row.querySelector('.p-check');
    return box && !box.disabled ? box : row.querySelector('.mill-name');
  }

  function keyOf(r) {
    const ref = rows[r];
    return typeof ref === 'number' ? 'f' + T.ids[ref] : 'd' + ref.path;
  }

  function setCursor(r) {
    cursorRow = r;
    cursorKey = r < rows.length ? keyOf(r) : '';
  }

  /** After the rows change, find the cursor row again, or the nearest shown folder above it. */
  function placeCursor() {
    const ref = !cursorKey ? undefined : cursorKey[0] === 'f' ? T.index.get(cursorKey.slice(1)) : T.dirs.get(cursorKey.slice(1));
    if (ref === undefined) {
      setCursor(0);
      return;
    }
    if (rows[cursorRow] === ref) return;
    let r = rows.indexOf(ref);
    for (let up = typeof ref === 'number' ? T.parent[ref] : ref.parent; r < 0 && up && up !== T.top; up = up.parent) r = rows.indexOf(up);
    setCursor(Math.max(0, r));
  }

  /** Keep exactly one tab stop in the tree: the cursor row, or the tree itself while that row is scrolled out. */
  function setTabStop() {
    const row = rowEl(cursorRow);
    const target = row ? rowTarget(row) : null;
    el.rows.querySelectorAll('[tabindex="0"]').forEach((c) => {
      if (c !== target) c.tabIndex = -1;
    });
    if (target) {
      target.tabIndex = 0;
      el.rows.removeAttribute('tabindex');
    } else if (rows.length) {
      el.rows.tabIndex = 0;
    } else {
      el.rows.removeAttribute('tabindex');
    }
  }

  /** Put focus back on the cursor row after a render dropped it, or park it on the tree while the row is scrolled out. */
  function focusCursor(act) {
    const a = document.activeElement;
    if (a && a !== el.rows && el.rows.contains(a)) return;
    const row = rowEl(cursorRow);
    if (row) {
      const same = act ? row.querySelector('[data-act="' + act + '"]') : null;
      (same && !same.disabled ? same : rowTarget(row)).focus({ preventScroll: true });
    } else if (rows.length && a !== el.rows) {
      parking = true;
      el.rows.tabIndex = 0;
      el.rows.focus({ preventScroll: true });
      parking = false;
    }
  }

  /** Move the tab stop and focus to row r, scrolling it into view. */
  function focusRow(r) {
    if (!rows.length) return;
    setCursor(Math.max(0, Math.min(rows.length - 1, r)));
    const h = rowHeight();
    const top = listTop + cursorRow * h;
    const view = el.tree.clientHeight;
    const scroll = el.tree.scrollTop;
    if (top < scroll) el.tree.scrollTop = cursorRow === 0 ? 0 : top;
    else if (view && top + h > scroll + view) el.tree.scrollTop = cursorRow === rows.length - 1 ? el.tree.scrollHeight : top + h - view;
    paintRows(false);
    setTabStop();
    const row = rowEl(cursorRow);
    if (row) rowTarget(row).focus({ preventScroll: true });
  }

  function pageRows() {
    return Math.max(1, Math.floor((el.tree.clientHeight || 400) / rowHeight()) - 1);
  }

  function setCollapsed(path, value) {
    if (value) S.collapsed.add(path);
    else S.collapsed.delete(path);
    renderTree();
  }

  function selectionChanged() {
    syncRows();
    renderChips();
    renderCounts();
    renderSource();
    renderOutput();
    renderRunState();
  }

  function syncSettingsInputs() {
    root.querySelectorAll('[data-setting]').forEach((input) => {
      input.checked = S.settings[input.dataset.setting] === input.value;
    });
    root.querySelectorAll('[data-opt]').forEach((input) => {
      input.checked = !!S.settings[input.dataset.opt];
    });
  }

  function renderSettingsState() {
    const s = S.settings;
    el.contentHint.hidden = s.content !== 'source';
    const on = OPTION_ROWS.filter((o) => o.key !== 'wrap' && s[o.key] && (o.key !== 'gitignore' || caps.gitignore)).length;
    const count = root.querySelector('[data-el="optscount"]');
    if (count) count.textContent = String(on);
    el.opts.classList.toggle('is-open', S.optsOpen);
    if (el.optsToggle) el.optsToggle.setAttribute('aria-expanded', String(S.optsOpen));
    el.insp.classList.toggle('is-open', S.inspOpen);
    const inspKey = root.querySelector('[data-act="insp"]');
    if (inspKey) inspKey.setAttribute('aria-expanded', String(S.inspOpen));
    el.sumText.textContent = (s.content === 'source' ? 'source' : 'readable') + ' · ' + s.format + ' · ' + plural(on, 'option');
  }

  function renderOutput() {
    const r = S.result;
    const stale = isStale();
    const issueN = S.issues.size;
    el.issueCount.hidden = !issueN;
    el.issueCount.textContent = String(issueN);
    el.views.querySelectorAll('[data-view]').forEach((tab) => {
      const on = tab.dataset.view === S.view;
      tab.setAttribute('aria-selected', String(on));
      tab.tabIndex = on ? 0 : -1;
    });
    root.querySelectorAll('[data-panel]').forEach((panel) => {
      panel.hidden = panel.dataset.panel !== S.view;
    });
    const ready = !!r && !stale && !S.busy;
    // Patched in place, so a focused key keeps its focus as the dump comes and goes.
    setAvailable(el.outCopy, ready);
    el.outCopy.classList.toggle('is-primary', ready);
    setAvailable(el.outDownload, ready);
    setAvailable(el.outTree, S.selected.size > 0 && !S.busy);
    const notes = [];
    if (r && stale && S.busy !== 'pack' && S.busy !== 'render') {
      notes.push('<div class="p-note">' + icon('alert') + '<div class="p-note-body"><b>Out of date.</b> Settings or ticks changed since this dump was made. Pulp again before copying.</div>' +
        '<button type="button" class="p-key is-sm" data-act="pulp">Pulp again</button></div>');
    }
    if (r && r.previewTruncated && !stale) {
      notes.push('<div class="p-note" style="--c: var(--label)">' + icon('files') + '<div class="p-note-body">Showing the first ' + human(r.dump.length) +
        ' of ' + human(r.dumpBytes) + '. Copy and Download include the whole dump.</div></div>');
    }
    const note = notes.join('');
    if (note !== lastNote) {
      el.outNote.innerHTML = note;
      lastNote = note;
    }
    el.outNote.hidden = !notes.length;
    renderDump();
    if (S.view === 'issues') renderIssues();
    renderDock();
    renderTabs();
  }

  function renderDump() {
    const r = S.result;
    if (!r) {
      if (shownDump !== BLANK_SHOWN) {
        el.dump.innerHTML = BLANK_LEDGER;
        el.dump.classList.add('is-blank');
        el.dump.scrollTop = 0;
        shownDump = BLANK_SHOWN;
      }
      el.watermark.hidden = false;
      el.watermark.innerHTML = watermarkHTML();
      el.dump.style.opacity = '';
      return;
    }
    el.watermark.hidden = true;
    if (shownDump !== r.dump) {
      el.dump.innerHTML = ledgerHTML(r.dump);
      el.dump.classList.remove('is-blank');
      el.dump.scrollTop = 0;
      shownDump = r.dump;
    }
    const stale = isStale();
    el.dump.classList.toggle('is-stale', stale);
    el.dump.style.opacity = S.busy === 'pack' ? '0.35' : stale ? '0.62' : '';
  }

  function watermarkHTML() {
    const touch = isTouch();
    if (S.busy === 'pack') {
      return esc('Pulping ' + plural(S.selected.size, 'file') + '…') + (touch ? '' : '<small>esc cancels</small>');
    }
    if (!S.files.length) return 'The dump lands here.<small>xml · markdown · plain text</small>';
    return esc(plural(S.selected.size, 'file') + ' ticked.') + '<small>press pulp' + (touch ? '' : ' · ' + PULP_HINT) + '</small>';
  }

  function renderTabs() {
    root.dataset.tab = S.tab;
    el.tabs.querySelectorAll('[data-tab]').forEach((b) => {
      const on = b.dataset.tab === S.tab;
      b.setAttribute('aria-selected', String(on));
      b.tabIndex = on ? 0 : -1;
    });
    const dot = root.querySelector('[data-el="tabdot"]');
    if (!dot) return;
    const tone = statusTone();
    dot.hidden = !S.result && S.busy !== 'pack';
    dot.style.setProperty('--c', tone === 'warn' ? 'var(--warn)' : tone === 'busy' ? 'var(--acc-text)' : tone === 'stale' ? 'var(--text-3)' : 'var(--ok)');
  }

  function statusTone() {
    if (S.busy === 'pack' || S.busy === 'render') return 'busy';
    if (!S.result) return 'idle';
    if (isStale()) return 'stale';
    return S.issues.size ? 'warn' : 'ok';
  }

  function renderRunState() {
    const busyPack = S.busy === 'pack';
    const n = S.selected.size;
    const text = root.querySelector('[data-el="pulptext"]');
    text.textContent = busyPack ? 'Pulping…' : n ? 'Pulp ' + plural(n, 'file') : 'Pulp';
    // While busy the key stays focusable, so keyboard focus survives the pack;
    // doPulp ignores it until the mill is idle.
    el.pulp.disabled = !n;
    if (S.busy) el.pulp.setAttribute('aria-disabled', 'true');
    else el.pulp.removeAttribute('aria-disabled');
    el.pulp.classList.toggle('is-primary', primary() === 'pulp' && !S.busy);
    if (el.pulpHint) el.pulpHint.hidden = !n || !!S.busy;
    const cancelFocused = document.activeElement === el.cancel;
    el.cancel.hidden = !busyPack;
    if (cancelFocused && !busyPack) el.pulp.focus({ preventScroll: true });
    el.progress.hidden = !(busyPack || S.busy === 'render');
    if (S.progress && S.progress.total) {
      el.progress.dataset.p = '';
      el.progress.style.setProperty('--p', String(S.progress.done / S.progress.total));
    } else {
      delete el.progress.dataset.p;
    }
    const tone = statusTone();
    const label = {
      busy: S.busy === 'render' ? 'Redrawing' : 'Pulping',
      idle: 'Not generated',
      stale: 'Out of date',
      warn: plural(S.issues.size, 'issue'),
      ok: 'Ready',
    }[tone];
    el.status.dataset.tone = tone;
    el.status.textContent = label;
    let why = '';
    if (busyPack) {
      why = S.progress && S.progress.total
        ? S.progress.done + ' of ' + S.progress.total + ' files'
        : 'Extracting ' + plural(n, 'file');
    } else if (tone === 'idle') {
      why = S.files.length ? plural(n, 'file') + ' ticked · ' + human(selectedBytes())
        : S.busy === 'scan' ? 'Scanning…' : S.scanned ? 'Nothing to pulp in this folder.' : 'Choose a folder first.';
    } else if (tone === 'stale') {
      why = 'Settings or ticks changed. Pulp again before copying.';
    } else if (tone === 'warn') {
      why = 'Flagged files are marked in the tree. Select one to see why.';
    }
    const r = S.result;
    const statMeta = root.querySelector('[data-el="statmeta"]');
    // The stats replace the explanation line once there is a dump.
    const showStats = !!r && !busyPack;
    el.why.textContent = showStats ? '' : why;
    if (showStats) {
      const skipped = Number(r.filesSkipped) || 0;
      const ms = Number(r.elapsedMs) || 0;
      el.stats.hidden = false;
      const stats = '<dt>files</dt><dd>' + (Number(r.filesExtracted) || 0) + (skipped ? ' <span class="p-meta">+' + skipped + ' skipped</span>' : '') + '</dd>' +
        '<dt>read</dt><dd>' + human(r.readBytes) + '</dd>' +
        '<dt>tokens</dt><dd>~' + tokensText(r.tokens) + '</dd>' +
        '<dt>time</dt><dd>' + (ms < 1 ? '&lt;1' : ms) + ' ms</dd>';
      if (stats !== lastStats) {
        el.stats.innerHTML = stats;
        lastStats = stats;
      }
      statMeta.textContent = '~' + tokensText(r.tokens) + ' tokens · ' + human(r.dumpBytes);
    } else {
      el.stats.hidden = true;
      statMeta.textContent = busyPack && S.progress && S.progress.total ? S.progress.done + ' of ' + S.progress.total + ' files'
        : tone === 'idle' && S.files.length ? plural(n, 'file') + ' ticked · ' + human(selectedBytes()) : '';
    }
  }

  function renderDock() {
    const p = primary();
    let html;
    if (S.busy === 'pack') {
      html = '<button type="button" class="p-key" data-act="cancel">' + icon('x') + 'Cancel</button>';
    } else if (p === 'source') {
      const off = S.busy ? ' aria-disabled="true"' : '';
      html = '<button type="button" class="p-key is-primary" data-act="browse"' + off + '>' + icon('folder') + (caps.source === 'path' ? 'Browse' : 'Choose folder') + '</button>' +
        (caps.sample ? '<button type="button" class="p-key" data-act="sample"' + off + '>' + icon('flask') + 'Sample</button>' : '');
    } else if (p === 'copy') {
      html = '<button type="button" class="p-key is-primary" data-act="copy">' + icon('copy') + 'Copy</button>' +
        '<button type="button" class="p-key" data-act="download">' + icon('download') + 'Save</button>' +
        '<button type="button" class="p-key is-sq" data-act="tree" aria-label="Copy the directory map" title="Copy the directory map">' + icon('tree') + '</button>';
    } else {
      const n = S.selected.size;
      html = '<button type="button" class="p-key is-primary" data-act="pulp"' + (n && !S.busy ? '' : ' aria-disabled="true"') + '>' + icon('press') + (n ? 'Pulp ' + plural(n, 'file') : 'Tick a file') + '</button>';
    }
    if (lastDock !== html) {
      // Cancel and Copy/Save/Tree trade places under the finger when a pack starts
      // or ends; a tap that lands just after the swap is ignored.
      if (lastDock.includes('data-act="cancel"') !== html.includes('data-act="cancel"')) dockArmedAt = performance.now() + 400;
      const hadFocus = el.dock.contains(document.activeElement);
      el.dock.innerHTML = html;
      lastDock = html;
      if (hadFocus) {
        const next = el.dock.querySelector('button');
        if (next) next.focus({ preventScroll: true });
      }
    }
  }

  function renderIssues() {
    // The list only changes with the result or the archives setting; ticks do not
    // redraw it, so a focused Untick or Report key keeps its focus.
    const shown = [S.issues, !!S.result, S.settings.archives];
    if (shownIssues && shown.every((v, j) => v === shownIssues[j])) return;
    shownIssues = shown;
    const list = [...S.issues.values()];
    if (!list.length) {
      el.issues.innerHTML = '<p class="mill-none">' + (S.result ? 'Every ticked file was extracted.' : 'Issues from the last pulp show up here.') + '</p>';
      return;
    }
    el.issues.innerHTML = list.map((o) => '<div class="mill-issue">' +
      '<button type="button" class="mill-issue-path mill-name" data-act="preview" data-id="' + esc(o.id) + '">' + icon('alert') + esc(o.relative) + '</button>' +
      '<span class="mill-issue-status">' + esc(o.status.replace(/_/g, ' ')) + '</span><p>' + esc(o.message || o.status) + '</p>' +
      '<div class="mill-issue-keys">' + issueKeys(o) + '</div></div>').join('');
  }

  function issueKeys(o, inNote) {
    const where = esc(o.relative || o.id);
    return '<button type="button" class="p-key is-sm" data-act="untick" data-id="' + esc(o.id) + '" aria-label="Untick ' + where + '">Untick</button>' +
      (o.status === 'skipped_archive' && !S.settings.archives ? '<button type="button" class="p-key is-sm" data-act="archives-on">Turn on archives</button>' : '') +
      '<button type="button" class="p-key is-sm is-quiet" data-act="report-issue" data-id="' + esc(o.id) + '" aria-label="Report ' + where + ' on GitHub">' + icon('github') + 'Report</button>' +
      (inNote ? '' : '<button type="button" class="p-key is-sm is-quiet" data-act="copy-issue" data-id="' + esc(o.id) + '" aria-label="Copy the report for ' + where + '">' + icon('copy') + 'Copy report</button>');
  }

  function renderPreview() {
    const p = S.preview;
    if (!p) {
      el.prevHead.hidden = false;
      el.prevHead.innerHTML = '<span>Select a file name in the tree to preview what pulp extracts from it.</span>';
      el.prevNote.hidden = true;
      el.prevBody.innerHTML = '';
      return;
    }
    const f = p.file;
    const mode = S.settings.content === 'source' ? 'source' : 'readable text';
    el.prevHead.innerHTML = '<b>' + esc(f.relative) + '</b><span>· ' + human(f.size) + ' · ' + mode + (p.truncated ? ' · first part only' : '') + '</span>' +
      (p.loading ? '<span class="p-spin" aria-label="Loading"></span>' : '');
    const issue = S.issues.get(fileId(f));
    el.prevHead.hidden = !!issue;
    if (issue) {
      el.prevNote.hidden = false;
      el.prevNote.innerHTML = '<div class="p-note" data-tone="' + (issue.status === 'error' || issue.status === 'unreadable' ? 'bad' : 'warn') + '">' +
        '<div class="p-note-body"><b>' + esc(f.relative + ' · ' + issue.status.replace(/_/g, ' ')) + '</b>' + esc(issue.message || '') +
        '<div class="p-note-actions">' + issueKeys(issue, true) + '</div></div></div>';
    } else {
      el.prevNote.hidden = true;
      el.prevNote.innerHTML = '';
    }
    if (p.loading) {
      el.prevBody.innerHTML = '<span class="ln mill-muted" data-n="1">extracting…</span>';
    } else if (p.error) {
      el.prevBody.innerHTML = '<span class="ln mill-muted" data-n="1">' + esc('Preview failed: ' + p.error) + '</span>';
    } else if (!p.text) {
      el.prevBody.innerHTML = '<span class="ln mill-muted" data-n="1">' + esc(issue || !p.message ? '(nothing extracted from this file)' : p.message) + '</span>';
    } else {
      el.prevBody.innerHTML = ledgerHTML(p.text);
    }
    el.prevBody.scrollTop = 0;
  }

  function renderAlert() {
    const a = S.alert;
    el.alert.hidden = !a;
    if (!a) {
      el.alert.innerHTML = '';
      return;
    }
    el.alert.innerHTML = '<div class="p-note" role="alert" data-tone="' + (a.tone || 'bad') + '">' + icon('alert') +
      '<div class="p-note-body"><b>' + esc(a.title) + '</b>' + (a.message ? '<pre>' + esc(a.message) + '</pre>' : '') +
      (a.reportable ? '<div class="p-note-actions"><button type="button" class="p-key is-sm" data-act="report-alert">' + icon('github') + 'Open GitHub issue</button>' +
        '<button type="button" class="p-key is-sm is-quiet" data-act="copy-alert">' + icon('copy') + 'Copy report</button></div>' : '') +
      '</div><button type="button" class="p-key is-quiet is-sm is-sq" data-act="dismiss" aria-label="Dismiss">' + icon('x') + '</button></div>';
  }

  function applyWrap() {
    el.dump.classList.toggle('is-wrap', !!S.settings.wrap);
    el.prevBody.classList.toggle('is-wrap', !!S.settings.wrap);
  }

  function onScreen(node) {
    return !!node && node.isConnected && node.getClientRects().length > 0;
  }

  /**
   * After a render hid or removed the focused control, move focus to the tree
   * or the Pulp key (whichever `prefer` names first), then the dock or the path.
   * `hadFocus` says whether focus was in the mill when the change began.
   */
  function rescueFocus(hadFocus, prefer) {
    if (!hadFocus) return;
    const a = document.activeElement;
    if (a && a !== document.body && root.contains(a) && onScreen(a)) return;
    const treeStop = el.rows.querySelector('[tabindex="0"]') || (el.rows.tabIndex === 0 ? el.rows : null);
    const emptyKey = el.filesEmpty.querySelector('[data-act]');
    const order = prefer === 'pulp'
      ? [el.pulp, treeStop, el.filesLoading, emptyKey]
      : [treeStop, el.filesLoading, emptyKey, el.pulp];
    order.push(el.dock.querySelector('button'), el.path);
    // A disabled key (Pulp with nothing ticked) is on screen but cannot take focus.
    const next = order.find((node) => onScreen(node) && !node.disabled);
    if (next) next.focus({ preventScroll: true });
  }

  function setBusy(kind) {
    S.busy = kind;
    if (!kind) S.progress = null;
    root.dataset.busy = kind || '';
    renderSource();
    renderFilesChrome();
    renderOutput();
    renderRunState();
  }

  function clearAlert() {
    if (!S.alert) return;
    S.alert = null;
    renderAlert();
  }

  function showAlert(title, err, reportable, tone) {
    S.alert = { title, message: err ? errText(err) : '', reportable: !!reportable, tone: tone || 'bad' };
    renderAlert();
  }

  function toast(text) {
    const t = document.createElement('div');
    t.className = 'p-toast';
    t.innerHTML = icon('check') + '<span>' + esc(text) + '</span>';
    el.toasts.appendChild(t);
    window.setTimeout(() => t.remove(), 2600);
  }

  /* ---------- reports ---------- */

  function millContext() {
    const s = S.settings;
    return [
      'surface: ' + (adapter.surfaceName || adapter.surface || 'mill'),
      'version: ' + (typeof adapter.version === 'function' ? adapter.version() : adapter.version || 'unknown'),
      'format: ' + s.format,
      'content: ' + s.content,
      'archives: ' + (s.archives ? 'on' : 'off'),
      'hidden: ' + (s.hidden ? 'on' : 'off'),
      'gitignore: ' + (caps.gitignore && s.gitignore ? 'on' : 'off'),
    ].join('\n');
  }

  function formatReport(report) {
    const title = ('mill: ' + (report.title || 'error')).slice(0, 120);
    const lines = [];
    if (report.relative) lines.push('path: ' + report.relative);
    if (report.status) lines.push('status: ' + report.status);
    if (report.kind) lines.push('kind: ' + report.kind);
    if (report.language) lines.push('language: ' + report.language);
    if (report.size != null && report.size !== '') lines.push('size_bytes: ' + report.size);
    const diagnostic = (lines.join('\n') + '\n\n' + (report.message || report.summary || '')).trim();
    const body = [
      '## What happened', '', report.summary || report.message || 'The mill reported an error.', '',
      '## Diagnostic', '', fence(diagnostic), '',
      '## Mill', '', fence(millContext()), '',
      'Files stayed on this machine. This report has the path and the error text only.',
    ].join('\n');
    return { title, body };
  }

  function openReport(report) {
    const url = (t, b) => ISSUES_URL + '?title=' + encodeURIComponent(t) + '&body=' + encodeURIComponent(b);
    let issue = formatReport(report);
    let href = url(issue.title, issue.body);
    if (href.length > 7000) {
      issue = formatReport(Object.assign({}, report, {
        message: String(report.message || report.summary || '').slice(0, 1200) + '\n\n… clipped for the GitHub form. Copy report has the full text.',
      }));
      href = url(issue.title, issue.body);
    }
    window.open(href, '_blank', 'noopener');
  }

  async function copyReport(report) {
    const issue = formatReport(report);
    try {
      await writeClipboard(issue.title + '\n\n' + issue.body);
      toast('Copied the report');
    } catch (err) {
      showAlert('Could not copy the report.', err, false);
    }
  }

  function outcomeReport(o) {
    return {
      title: (o.status || 'error') + ' ' + (o.relative || 'file'),
      summary: (o.status || 'error') + ' while packing `' + (o.relative || 'a file') + '`.',
      relative: o.relative,
      status: o.status,
      kind: o.kind,
      language: o.language,
      size: o.size,
      message: o.message || o.status || '',
    };
  }

  function alertReport() {
    const a = S.alert || {};
    const text = (a.title || '') + (a.message ? '\n' + a.message : '');
    return { title: String(a.title || 'error').slice(0, 100), summary: text, message: text };
  }

  /* ---------- actions ---------- */

  function newSource() {
    S.sourceId++;
    S.previewSeq++;
    S.result = null;
    S.issues = new Map();
    flagDirs = new Set();
    S.preview = null;
    S.active = '';
    S.view = 'combined';
    shownDump = null;
  }

  /** `focusHint` carries focus from a sample or a pick that ran just before. */
  async function doScan(keepSelection, focusHint) {
    if (S.busy) return;
    const hadFocus = !!focusHint || root.contains(document.activeElement);
    if (caps.source === 'path' && !S.path) {
      showAlert('Give the mill a folder.', null, false, 'warn');
      el.path && el.path.focus();
      return;
    }
    clearAlert();
    const key = discoveryKey();
    S.scanTried = key;
    const request = {
      path: S.path,
      gitignore: caps.gitignore ? S.settings.gitignore : false,
      hidden: S.settings.hidden,
      archives: S.settings.archives,
    };
    setBusy('scan');
    rescueFocus(hadFocus, 'tree');
    try {
      const data = await adapter.scan(request);
      const files = data.files || [];
      const before = keepSelection ? T.index : null;
      const next = new Set();
      for (const f of files) {
        const id = fileId(f);
        if (f.oversized) continue;
        if (before && before.has(id)) {
          if (S.selected.has(id)) next.add(id);
        } else if (f.default_on !== false) {
          next.add(id);
        }
      }
      S.files = files;
      S.byId = new Map(files.map((f) => [fileId(f), f]));
      S.selected = next;
      selVersion++;
      T = buildModel(files);
      applyFilter();
      if (!keepSelection) S.collapsed = new Set(files.length > LARGE_TREE ? T.dirs.keys() : []);
      S.scanKey = key;
      S.scanned = true;
      if (data.label) S.sourceLabel = data.label;
      if (caps.source === 'path' && !demo && S.remember !== false) writeStore(PATH_KEY, S.path);
      if (!keepSelection) S.tab = 'files';
      renderFiles();
      if (data.truncated) {
        showAlert('The scan stopped at the size cap, so some files were left out.', 'Narrow the folder, or use the CLI with --max-total-bytes.', false, 'warn');
      }
    } catch (err) {
      if (!keepSelection) {
        S.files = [];
        S.byId = new Map();
        S.selected = new Set();
        selVersion++;
        T = buildModel([]);
        S.scanned = false;
        renderFiles();
      }
      failed('Scan failed.', err);
    } finally {
      setBusy('');
    }
    rescueFocus(hadFocus, 'tree');
    settle();
  }

  /** Busy answers from the local mill are a wait, not a bug worth reporting. */
  function failed(title, err) {
    if (err && err.busy) showAlert('The mill is busy.', err, false, 'warn');
    else if (err && err.reload) showAlert('This mill has restarted.', err, false, 'warn');
    else if (err && err.user) showAlert(title, err, false, 'warn');
    else showAlert(title, err, true);
  }

  /** Replay setting changes that arrived while the mill was busy. */
  function settle() {
    // A scan that found nothing still has settings worth rescanning with.
    if (S.busy || !S.scanned) return;
    const dk = discoveryKey();
    if (dk !== S.scanKey && dk !== S.scanTried) {
      doScan(true);
      return;
    }
    if (caps.rerender && S.result && isStale() && S.result.extractKey === extractKey() && sameSelection(S.result.picked) &&
      packKey() !== S.renderTried) {
      doRerender();
    }
  }

  async function doBrowse(kind) {
    if (S.busy) return;
    const hadFocus = root.contains(document.activeElement);
    clearAlert();
    const hold = caps.source === 'path';
    if (hold) setBusy('browse');
    let picked;
    try {
      picked = kind === 'files' ? await adapter.pickFiles() : await adapter.browse();
    } catch (err) {
      if (hold) setBusy('');
      failed(kind === 'files' ? 'Could not open the file picker.' : 'Could not open the folder picker.', err);
      return;
    }
    if (hold) setBusy('');
    await takeSource(picked, hadFocus);
  }

  async function takeSource(picked, hadFocus) {
    if (!picked || picked.cancelled) {
      if (picked && picked.cancelled) toast(picked.empty ? 'No files there' : 'Nothing chosen');
      return;
    }
    if (S.busy) return;
    if (picked.path != null) {
      S.path = picked.path;
      // A source may name itself (the sample does) instead of showing a temp path.
      S.pathShown = picked.display || picked.path;
      showPathEnd(S.pathShown);
    }
    S.remember = !picked.sample;
    if (picked.label) S.sourceLabel = picked.label;
    newSource();
    await doScan(false, hadFocus);
  }

  async function doSample() {
    if (S.busy || !adapter.sample) return;
    const hadFocus = root.contains(document.activeElement);
    clearAlert();
    setBusy('sample');
    // The empty state and its sample key hide while the sample loads.
    rescueFocus(hadFocus, 'tree');
    let picked;
    try {
      picked = await adapter.sample();
    } catch (err) {
      setBusy('');
      failed('Could not load the sample project.', err);
      return;
    }
    setBusy('');
    await takeSource(picked, hadFocus);
    rescueFocus(hadFocus, 'tree');
  }

  async function doPulp() {
    if (S.busy || !S.selected.size) return;
    clearAlert();
    if (discoveryKey() !== S.scanKey) await doScan(true);
    if (S.busy || discoveryKey() !== S.scanKey || !S.selected.size) return;
    const req = packRequest();
    const key = packKey();
    const ek = extractKey();
    const picked = new Set(req.selected);
    const readBytes = selectedBytes();
    S.progress = caps.progress ? { done: 0, total: req.selected.length } : null;
    const hadFocus = root.contains(document.activeElement);
    setBusy('pack');
    // "Pulp again" leaves with the out-of-date note; focus goes to the Pulp key.
    rescueFocus(hadFocus, 'pulp');
    try {
      const res = await adapter.pack(req, {
        onProgress(done, total) {
          S.progress = { done, total };
          renderRunState();
        },
      });
      if (res.cancelled) {
        toast('Pulp cancelled');
        return;
      }
      applyResult(res, key, ek, picked, true, readBytes);
      S.tab = 'output';
      if (res.error && !res.filesExtracted) {
        showAlert('Nothing was extracted.', res.error, true, 'warn');
      }
    } catch (err) {
      if (isCancel(err)) toast('Pulp cancelled');
      else failed('Pack failed.', err);
    } finally {
      setBusy('');
    }
    rescueFocus(hadFocus, 'pulp');
    settle();
  }

  /**
   * Show a pack or redraw result. `picked` is the ticked set it was made from and
   * `readBytes` their size; a redraw keeps the pack's timing.
   */
  function applyResult(res, key, ek, picked, fresh, readBytes) {
    const prev = S.result;
    const elapsedMs = res.elapsedMs || (!fresh && prev ? prev.elapsedMs : 0);
    S.result = Object.assign({}, res, { key, extractKey: ek, picked, readBytes, elapsedMs });
    S.issues = new Map();
    for (const o of res.outcomes || []) {
      if (o.status && o.status !== 'extracted') S.issues.set(o.id || o.relative, Object.assign({ id: o.id || o.relative }, o));
    }
    flagDirs = flaggedFolders();
    if (fresh) S.view = S.view === 'issues' && S.issues.size ? 'issues' : 'combined';
    if (S.view === 'issues' && !S.issues.size) S.view = 'combined';
    if (S.view === 'preview' && !S.preview) S.view = 'combined';
    if (S.preview && !S.preview.loading) renderPreview();
    // A fresh pack opens the folders that hold flagged files, so the flags show.
    let opened = false;
    if (fresh) {
      for (const path of flagDirs) opened = S.collapsed.delete(path) || opened;
    }
    if (opened) renderTree();
    else paintRows(true);
    renderChips();
  }

  /** Every folder above a flagged file. */
  function flaggedFolders() {
    const out = new Set();
    for (const o of S.issues.values()) {
      const i = T.index.get(o.id);
      if (i !== undefined) {
        for (let d = T.parent[i]; d && d !== T.top; d = d.parent) out.add(d.path);
        continue;
      }
      // Members of an unpacked archive have no row; flag the folders on their path.
      const parts = String(o.relative || '').split('/');
      for (let k = 1; k < parts.length; k++) {
        const path = parts.slice(0, k).join('/');
        if (T.dirs.has(path)) out.add(path);
      }
    }
    return out;
  }

  async function doRerender() {
    const req = packRequest();
    const key = packKey();
    const ek = extractKey();
    const picked = new Set(req.selected);
    const readBytes = selectedBytes();
    S.renderTried = key;
    setBusy('render');
    try {
      const res = await adapter.render(req);
      applyResult(res, key, ek, picked, false, readBytes);
    } catch (err) {
      failed('Could not redraw the dump.', err);
    } finally {
      setBusy('');
    }
    settle();
  }

  async function doCancel() {
    if (S.busy !== 'pack') return;
    try {
      await adapter.cancel();
    } catch (_) {
      // The pack settles on its own; a failed cancel request changes nothing.
    }
  }

  async function doPreview(id) {
    const f = S.byId.get(id);
    if (!f) return;
    S.active = id;
    S.view = 'preview';
    S.tab = 'output';
    const seq = ++S.previewSeq;
    S.preview = { file: f, loading: true };
    syncRows();
    renderOutput();
    renderPreview();
    try {
      const p = await previewWhenFree(f, seq);
      if (seq !== S.previewSeq) return;
      S.preview = Object.assign({ file: f }, p);
    } catch (err) {
      if (seq !== S.previewSeq) return;
      S.preview = { file: f, error: errText(err) };
    }
    renderPreview();
  }

  /**
   * The local mill extracts one preview at a time and answers busy to the rest.
   * The newest click waits for the slot instead of failing; older ones give up.
   */
  async function previewWhenFree(f, seq) {
    for (let tries = 0; ; tries++) {
      try {
        return await adapter.preview(f, packRequest());
      } catch (err) {
        if (!(err && err.busy) || tries >= 100 || seq !== S.previewSeq) throw err;
        await new Promise((resolve) => window.setTimeout(resolve, 150));
        if (seq !== S.previewSeq) throw err;
      }
    }
  }

  async function fullDump() {
    const r = S.result;
    if (!r) return '';
    if (!r.previewTruncated) return r.dump;
    return adapter.fullDump(r, packRequest());
  }

  async function doCopy() {
    const r = S.result;
    if (!r || isStale()) return;
    try {
      // No await before the write: the text, or its promise, goes over inside the click.
      await writeClipboard(r.previewTruncated ? adapter.fullDump(r, packRequest()) : r.dump);
      toast('Copied ' + r.filename + ' · ' + human(r.dumpBytes));
    } catch (err) {
      failed('Copy failed.', err);
    }
  }

  async function doDownload() {
    if (!S.result || isStale()) return;
    try {
      const text = await fullDump();
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([text], { type: 'text/plain;charset=utf-8' }));
      a.download = S.result.filename;
      document.body.appendChild(a);
      a.click();
      a.remove();
      window.setTimeout(() => URL.revokeObjectURL(a.href), 1000);
      toast('Saved ' + S.result.filename);
    } catch (err) {
      failed('Download failed.', err);
    }
  }

  async function doTree() {
    if (!S.selected.size) return;
    try {
      await writeClipboard(Promise.resolve(adapter.tree(packRequest())).then((t) => t.text));
      toast('Copied the directory map');
    } catch (err) {
      failed('Could not copy the directory map.', err);
    }
  }

  function setSetting(name, value) {
    if (S.settings[name] === value) return;
    S.settings[name] = value;
    if (!demo) writeStore(SETTINGS_KEY, S.settings);
    renderSettingsState();
    if (name === 'wrap') {
      applyWrap();
      return;
    }
    if (S.view === 'preview' && S.active && name === 'content' && !S.busy) doPreview(S.active);
    renderOutput();
    renderRunState();
    if (DISCOVERY.includes(name) || name === 'format' || name === 'tree') settle();
  }

  function setAll(on) {
    for (let i = 0; i < T.n; i++) if (T.ok[i] && isShown(i)) tick(i, on);
    selectionChanged();
  }

  function toggleDir(path, on) {
    const under = [];
    const stack = T.dirs.has(path) ? [T.dirs.get(path)] : [];
    while (stack.length) {
      const node = stack.pop();
      for (const i of node.files) if (T.ok[i] && isShown(i)) under.push(i);
      for (const d of node.kids) stack.push(d);
    }
    // Tick in scan order so the ticked set keeps the order it always had.
    for (const i of Int32Array.from(under).sort()) tick(i, on);
    selectionChanged();
  }

  function toggleLang(lang) {
    const l = T.langIds.get(lang);
    if (l !== undefined) {
      const allOn = T.langOn[l] === T.langTotal[l];
      for (const i of T.byLang[l]) if (T.ok[i] && isShown(i)) tick(i, !allOn);
    }
    selectionChanged();
  }

  /* ---------- events ---------- */

  root.addEventListener('click', (e) => {
    const t = e.target.closest('[data-act]');
    if (!t || !root.contains(t) || t.disabled || t.getAttribute('aria-disabled') === 'true') return;
    if (t.closest('.mill-dock') && performance.now() < dockArmedAt) return;
    const act = t.dataset.act;
    if (act === 'check' || act === 'dircheck') return;
    const row = t.closest('.mill-row');
    const id = t.dataset.id || (row && row.dataset.id);
    switch (act) {
      case 'browse': doBrowse('folder'); break;
      case 'pick-files': doBrowse('files'); break;
      case 'scan':
        if (S.busy) break;
        pathFromField();
        newSource();
        doScan(false);
        break;
      case 'sample': doSample(); break;
      case 'pulp': doPulp(); break;
      case 'cancel': doCancel(); break;
      case 'copy': doCopy(); break;
      case 'download': doDownload(); break;
      case 'tree': doTree(); break;
      case 'all': setAll(true); break;
      case 'none': setAll(false); break;
      case 'chip': toggleLang(t.dataset.lang); break;
      case 'twist': {
        const dir = row && row.dataset.dir;
        if (dir == null) break;
        setCollapsed(dir, !S.collapsed.has(dir));
        break;
      }
      case 'preview': if (id) doPreview(id); break;
      case 'untick':
        if (id) {
          if (T.index.has(id)) tick(T.index.get(id), false);
          selectionChanged();
        }
        break;
      case 'archives-on':
        setSetting('archives', true);
        syncSettingsInputs();
        break;
      case 'report-issue': if (id && S.issues.has(id)) openReport(outcomeReport(S.issues.get(id))); break;
      case 'copy-issue': if (id && S.issues.has(id)) copyReport(outcomeReport(S.issues.get(id))); break;
      case 'report-alert': openReport(alertReport()); break;
      case 'copy-alert': copyReport(alertReport()); break;
      case 'dismiss':
        S.alert = null;
        renderAlert();
        break;
      case 'opts':
        S.optsOpen = !S.optsOpen;
        renderSettingsState();
        break;
      case 'insp':
        S.inspOpen = !S.inspOpen;
        renderSettingsState();
        break;
      default: break;
    }
  });

  root.addEventListener('change', (e) => {
    const t = e.target;
    if (t.matches('[data-act="check"]')) {
      const id = t.closest('.mill-row').dataset.id;
      if (T.index.has(id)) tick(T.index.get(id), t.checked);
      selectionChanged();
    } else if (t.matches('[data-act="dircheck"]')) {
      toggleDir(t.closest('.mill-row').dataset.dir, t.checked);
    } else if (t.matches('[data-setting]')) {
      if (t.checked) setSetting(t.dataset.setting, t.value);
    } else if (t.matches('[data-opt]')) {
      setSetting(t.dataset.opt, t.checked);
    }
  });

  let filterTimer = 0;
  el.filter.addEventListener('input', () => {
    window.clearTimeout(filterTimer);
    filterTimer = window.setTimeout(() => {
      S.filter = el.filter.value.trim().toLowerCase();
      applyFilter();
      renderChips();
      renderTree();
      renderCounts();
    }, 80);
  });

  /* Tree keys: arrows move between rows, Right and Left open, close, and climb
     folders, Enter previews a file or opens a folder, Space ticks natively. */
  el.rows.addEventListener('keydown', (e) => {
    if (e.altKey || e.ctrlKey || e.metaKey || !rows.length) return;
    const onTree = e.target === el.rows;
    const row = onTree ? null : e.target.closest('.mill-row');
    const r = onTree ? cursorRow : row ? rowIndex(row) : -1;
    if (r < 0) return;
    const ref = rows[r];
    const dir = typeof ref !== 'number';
    let next = null;
    switch (e.key) {
      case 'ArrowDown': next = r + 1; break;
      case 'ArrowUp': next = r - 1; break;
      case 'Home': next = 0; break;
      case 'End': next = rows.length - 1; break;
      case 'PageDown': next = r + pageRows(); break;
      case 'PageUp': next = r - pageRows(); break;
      case 'ArrowRight':
        if (dir && isCollapsed(ref.path)) setCollapsed(ref.path, false);
        else if (dir && r + 1 < rows.length && depthOf(r + 1) > ref.depth) next = r + 1;
        break;
      case 'ArrowLeft':
        if (dir && !S.filter && !S.collapsed.has(ref.path)) setCollapsed(ref.path, true);
        else if (parentRow(r) >= 0) next = parentRow(r);
        break;
      case 'Enter':
      case ' ':
        // Focus parked on the tree comes back to its row first; otherwise Space
        // ticks the box and a focused button clicks itself.
        if (onTree) next = r;
        else if (e.key === ' ' || e.target.tagName === 'BUTTON') return;
        else if (dir) setCollapsed(ref.path, !S.collapsed.has(ref.path));
        else doPreview(T.ids[ref]);
        break;
      default:
        return;
    }
    e.preventDefault();
    if (next !== null) focusRow(next);
  });

  el.rows.addEventListener('focusin', (e) => {
    if (e.target !== el.rows) {
      const row = e.target.closest('.mill-row');
      const r = row ? rowIndex(row) : -1;
      if (r >= 0 && r !== cursorRow) {
        setCursor(r);
        setTabStop();
      }
      return;
    }
    if (parking) return;
    // Keyboard focus lands on the tree itself only while the cursor row is
    // scrolled out: bring that row back. A click on a row's text lands here
    // too: that row takes focus. Anything else leaves the view where it is.
    if (el.rows.matches(':focus-visible')) focusRow(cursorRow);
    else if (pointerRow && rowIndex(pointerRow) >= 0) focusRow(rowIndex(pointerRow));
  });

  el.rows.addEventListener('pointerdown', (e) => {
    pointerRow = e.target.closest('.mill-row');
    window.setTimeout(() => {
      pointerRow = null;
    }, 0);
  });

  el.tree.addEventListener('scroll', () => {
    if (rows.length > FULL_TREE) paintRows(false);
  }, { passive: true });

  if (typeof ResizeObserver === 'function') {
    const resized = new ResizeObserver(() => {
      rowH = 0;
      if (rows.length > FULL_TREE) paintRows(false);
    });
    resized.observe(el.tree);
    unlisten.push(() => resized.disconnect());
  }

  if (el.path) {
    el.path.addEventListener('blur', scrollPathEnd);
    el.path.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        if (S.busy) return;
        pathFromField();
        newSource();
        doScan(false);
      }
    });
  }

  el.tabs.addEventListener('click', (e) => {
    const b = e.target.closest('[data-tab]');
    if (!b) return;
    S.tab = b.dataset.tab;
    renderTabs();
    if (el.path && document.activeElement !== el.path) scrollPathEnd();
  });

  // WebKit drops :focus-visible while arrow keys move through a radio group, so a
  // group steered from the keyboard is marked until the next pointer press.
  root.addEventListener('keydown', (e) => {
    if (!e.key.startsWith('Arrow')) return;
    const seg = e.target.closest && e.target.closest('.p-seg');
    if (seg) seg.classList.add('is-kbd');
  });
  root.addEventListener('pointerdown', () => {
    root.querySelectorAll('.p-seg.is-kbd').forEach((seg) => seg.classList.remove('is-kbd'));
  });

  el.tabs.addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    S.tab = S.tab === 'files' ? 'output' : 'files';
    renderTabs();
    const next = el.tabs.querySelector('[data-tab="' + S.tab + '"]');
    if (next) next.focus();
  });

  el.views.addEventListener('click', (e) => {
    const b = e.target.closest('[data-view]');
    if (!b) return;
    selectView(b.dataset.view);
  });

  el.views.addEventListener('keydown', (e) => {
    const tabs = [...el.views.querySelectorAll('[data-view]')];
    const i = tabs.findIndex((t) => t.dataset.view === S.view);
    const step = { ArrowRight: i + 1, ArrowLeft: i + tabs.length - 1, Home: 0, End: tabs.length - 1 }[e.key];
    if (step === undefined) return;
    e.preventDefault();
    const next = tabs[step % tabs.length];
    selectView(next.dataset.view);
    next.focus();
  });

  function selectView(view) {
    S.view = view;
    renderOutput();
    if (view === 'preview') renderPreview();
  }

  // The options popover closes once focus leaves it.
  root.querySelector('.mill-set-opts').addEventListener('focusout', (e) => {
    if (S.optsOpen && !e.currentTarget.contains(e.relatedTarget)) {
      S.optsOpen = false;
      renderSettingsState();
    }
  });

  if (!demo) {
    listen(document, 'keydown', (e) => {
      const t = e.target;
      // Checkboxes and radios take no text, so / still reaches the filter from the tree.
      const typing = !!t && ((t.tagName === 'INPUT' && t.type !== 'checkbox' && t.type !== 'radio') ||
        t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
      if (e.key === 'Escape') {
        if (S.busy === 'pack') {
          e.preventDefault();
          doCancel();
        } else if (S.optsOpen) {
          const inside = el.opts.contains(document.activeElement);
          S.optsOpen = false;
          renderSettingsState();
          if (inside && el.optsToggle) el.optsToggle.focus();
        } else if (S.inspOpen && onScreen(root.querySelector('[data-act="insp"]'))) {
          const inside = el.insp.contains(document.activeElement);
          S.inspOpen = false;
          renderSettingsState();
          if (inside) root.querySelector('[data-act="insp"]').focus();
        }
        return;
      }
      if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        doPulp();
        return;
      }
      if (typing) return;
      if (e.key === '/' && !e.metaKey && !e.ctrlKey && !e.altKey && S.files.length) {
        e.preventDefault();
        S.tab = 'files';
        renderTabs();
        el.filter.focus();
      }
    });
    listen(document, 'click', (e) => {
      if (S.optsOpen && !e.target.closest('.mill-set-opts')) {
        S.optsOpen = false;
        renderSettingsState();
      }
    });
  }

  if (caps.drop && !demo && adapter.drop) {
    let depth = 0;
    const hasFiles = (e) => !!e.dataTransfer && Array.prototype.indexOf.call(e.dataTransfer.types || [], 'Files') !== -1;
    listen(window, 'dragenter', (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth++;
      el.drop.hidden = false;
    });
    listen(window, 'dragleave', (e) => {
      if (!hasFiles(e)) return;
      depth = Math.max(0, depth - 1);
      if (!depth || e.clientX <= 0 || e.clientY <= 0 || e.clientX >= window.innerWidth || e.clientY >= window.innerHeight) {
        depth = 0;
        el.drop.hidden = true;
      }
    });
    listen(window, 'dragover', (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
    });
    listen(window, 'drop', (e) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth = 0;
      el.drop.hidden = true;
      if (S.busy) return;
      const pending = adapter.drop(e.dataTransfer);
      setBusy('browse');
      Promise.resolve(pending).then((picked) => {
        setBusy('');
        return takeSource(picked);
      }, (err) => {
        setBusy('');
        failed('Could not read the dropped files.', err);
      });
    });
  }

  return {
    scan: () => doScan(false),
    pulp: doPulp,
    sample: doSample,
    showError: (title, err, reportable) => showAlert(title, err, reportable),
    /** Remove the listeners the mill put on the document and window. */
    destroy() {
      while (unlisten.length) unlisten.pop()();
    },
    state: S,
  };
}

