/* tslint:disable */
/* eslint-disable */

/**
 * A scan's filters, compiled once, for a page that walks a granted folder
 * itself: which folders it can leave unopened because the filters leave out
 * every file inside them (`target/`, `node_modules/`, hidden folders while
 * hidden files are off). Skipping them changes no scan, only its time.
 */
export class FolderFilter {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Whether a walk should open the folder at `relative`, a path inside
     * the grant folder.
     */
    enter(relative: string): boolean;
    /**
     * The filters of a scan with `{ hidden, archives, exclude?,
     * no_default_excludes? }`, the settings [`scan_keep`] takes.
     */
    constructor(input: any);
}

/**
 * Full dump for a previous [`pack_files`] result, in the format it was packed in.
 */
export function artifact(result_id: string): string;

/**
 * Full dump for a stored result, `{ result_id, format, no_tree }`.
 */
export function artifact_as(input: any): string;

/**
 * Full dump for a stored result, `{ result_id, format, no_tree }`, passed to
 * `sink` as `Uint8Array` chunks of UTF-8. Returns the dump's length in bytes.
 */
export function artifact_chunks(input: any, sink: Function): number;

/**
 * Forget a stored result. Returns whether it was still held.
 */
export function drop_result(result_id: string): boolean;

/**
 * Extract a batch of a stepped pack: `{ root, files: [{ relative, id?, size,
 * kind?, bytes | failure }], ...settings }`. Returns the batch as JSON bytes
 * (a `Uint8Array`) for [`pack_add`]: an archive can expand to more text than
 * one JS string holds. A file with a `failure` ({ reason, message }) gets the
 * note its failure calls for. Stores nothing, so it can run in a spare
 * instance.
 */
export function extract_files(input: any): Uint8Array;

/**
 * How long the mill gives one [`heavy_kind`] file before it stops its
 * extractor, in milliseconds: `pulp ui`'s limit for a child process.
 */
export function extract_timeout_ms(): number;

/**
 * Directory map for `paths` in the dump format. Reads no file bytes.
 */
export function format_tree(input: any): string;

/**
 * Whether files of `kind` (a label such as `pdf` or `docx`, as a scan
 * reports it) have the parsers `pulp ui` runs in a child process with a time
 * limit. The mill runs them on their own, under [`extract_timeout_ms`].
 */
export function heavy_kind(kind: string): boolean;

/**
 * Whether extracting a file of `kind` can run a parser [`heavy_kind`]
 * covers: the file is of a heavy kind, or it is an archive a pack with
 * `archives` on expands, whose members may be. The page never runs such a
 * parser on its own thread, where nothing could stop one that hangs.
 */
export function needs_worker(kind: string, archives: boolean): boolean;

/**
 * Drop the pack [`pack_begin`] started, as a cancelled or failed pack
 * leaves it. The result it would have replaced stays as it was.
 */
export function pack_abort(): void;

/**
 * Add a batch from [`extract_files`], its JSON bytes, to the pack
 * [`pack_begin`] started. A batch that fails to add leaves the pack as it
 * was, so its files can be noted instead.
 */
export function pack_add(json: Uint8Array): void;

/**
 * Start a stepped pack of `{ files: [{ relative, id?, size, kind?,
 * modified?, scanned? }], truncated?, replaces?, ...settings }`, where `kind`
 * is the scan's kind for the file, `size` and `modified` are the file as the
 * page just read it, `scanned` the size the scan listed it at, `truncated`
 * says the scan was cut at its budgets, and `replaces` names the result on
 * screen. Returns the grant root and the files
 * to extract, in order, with their names inside the root, their kinds,
 * whether each goes alone, and whether it is `cached`: taken from the
 * replaced result, so the page neither reads nor extracts it. The files are
 * ticked already; nothing else is selected.
 */
export function pack_begin(input: any): any;

/**
 * Pack selected files. Each file is `{ relative, bytes: Uint8Array, id? }`.
 *
 * The extracted files stay in this instance under `result_id`, so
 * [`render_result`] and [`artifact_as`] can draw them again.
 */
export function pack_files(input: any): any;

/**
 * End the pack [`pack_begin`] started: store it as a result and return what
 * [`pack_files`] returns. The result it replaces is dropped.
 */
export function pack_finish(): any;

/**
 * Text pulp extracts from one file, `{ relative, bytes: Uint8Array }` plus the
 * pack settings that change it (`source`, `notebook_outputs`, `archives`,
 * `hidden`, ...), capped at 32 KiB. A file the browser could not read comes
 * as `{ relative, size, failure: { reason, message } }` and gets the status
 * and message a pack would give it. Stores nothing.
 */
export function preview_file(input: any): any;

export function pulp_version(): string;

/**
 * Redraw a stored result, `{ result_id, format, no_tree }`, without extracting
 * again. Returns what [`pack_files`] returns, under the same `result_id`.
 */
export function render_result(input: any): any;

/**
 * Classify a file list without reading contents (extension / name based).
 * A file with a `head` is also classified by its leading bytes.
 */
export function scan_files(input: any): any;

/**
 * Which of a grant's paths the scan's filters keep: `{ relatives, hidden,
 * archives }` to `{ root, keep }`. Reads no file, so a page can learn which
 * picked files it needs the size of before it asks the browser for any.
 */
export function scan_keep(input: any): any;

/**
 * Indices of the files in a scan input that [`scan_files`] would list but
 * cannot classify by name. Read their leading bytes into `head` and scan again.
 */
export function scan_unknown(input: any): Uint32Array;

/**
 * Tiny self-check used by the mill on load (pure Rust, no JS byte marshaling).
 */
export function smoke_pack(): any;

export function start(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_folderfilter_free: (a: number, b: number) => void;
    readonly artifact: (a: number, b: number) => [number, number, number, number];
    readonly artifact_as: (a: any) => [number, number, number, number];
    readonly artifact_chunks: (a: any, b: any) => [number, number, number];
    readonly drop_result: (a: number, b: number) => number;
    readonly extract_files: (a: any) => [number, number, number, number];
    readonly extract_timeout_ms: () => number;
    readonly folderfilter_enter: (a: number, b: number, c: number) => number;
    readonly folderfilter_new: (a: any) => [number, number, number];
    readonly format_tree: (a: any) => [number, number, number, number];
    readonly heavy_kind: (a: number, b: number) => number;
    readonly needs_worker: (a: number, b: number, c: number) => number;
    readonly pack_abort: () => void;
    readonly pack_add: (a: number, b: number) => [number, number];
    readonly pack_begin: (a: any) => [number, number, number];
    readonly pack_files: (a: any) => [number, number, number];
    readonly pack_finish: () => [number, number, number];
    readonly preview_file: (a: any) => [number, number, number];
    readonly pulp_version: () => [number, number];
    readonly render_result: (a: any) => [number, number, number];
    readonly scan_files: (a: any) => [number, number, number];
    readonly scan_keep: (a: any) => [number, number, number];
    readonly scan_unknown: (a: any) => [number, number, number, number];
    readonly smoke_pack: () => [number, number, number];
    readonly start: () => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
