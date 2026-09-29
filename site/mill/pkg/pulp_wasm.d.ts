/* tslint:disable */
/* eslint-disable */

/**
 * Full dump for a previous [`pack_files`] result, in the format it was packed in.
 */
export function artifact(result_id: string): string;

/**
 * Full dump for a stored result, `{ result_id, format, no_tree }`.
 */
export function artifact_as(input: any): string;

/**
 * Forget a stored result. Returns whether it was still held.
 */
export function drop_result(result_id: string): boolean;

/**
 * Directory map for `paths` in the dump format. Reads no file bytes.
 */
export function format_tree(input: any): string;

/**
 * Pack selected files. Each file is `{ relative, bytes: Uint8Array, id? }`.
 *
 * The extracted files stay in this instance under `result_id`, so
 * [`render_result`] and [`artifact_as`] can draw them again.
 */
export function pack_files(input: any): any;

/**
 * Text pulp extracts from one file, `{ relative, bytes: Uint8Array }` plus the
 * pack settings that change it (`source`, `notebook_outputs`, `archives`,
 * `hidden`, ...), capped at 32 KiB. Stores nothing.
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
 */
export function scan_files(input: any): any;

/**
 * Tiny self-check used by the mill on load (pure Rust, no JS byte marshaling).
 */
export function smoke_pack(): any;

export function start(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly artifact: (a: number, b: number) => [number, number, number, number];
    readonly artifact_as: (a: any) => [number, number, number, number];
    readonly drop_result: (a: number, b: number) => number;
    readonly format_tree: (a: any) => [number, number, number, number];
    readonly pack_files: (a: any) => [number, number, number];
    readonly preview_file: (a: any) => [number, number, number];
    readonly pulp_version: () => [number, number];
    readonly render_result: (a: any) => [number, number, number];
    readonly scan_files: (a: any) => [number, number, number];
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
