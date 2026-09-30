# Browser WASM mill

Status: **shipped** at https://pulp.onlygass.dev/mill

The browser mill packs files you grant in the tab. Native `pulp ui` still walks the filesystem with gitignore and isolates hang-prone parsers.

## Capabilities

| Capability | Browser mill | Native mill (`pulp ui`) |
| --- | --- | --- |
| Files never leave the device | yes | yes |
| Choose folder / choose files | the browser's folder and file pickers where it has them (Chrome, Edge), else the folder and file inputs; or drop a folder or files | OS file manager, or a typed path |
| Folders the filters leave out (`node_modules/`, `target/`, hidden folders) | never opened by a walk of a picked or dropped folder; the folder input lists every file first | never walked |
| Type an arbitrary filesystem path | no | yes |
| `.gitignore` discovery | no | yes |
| Default exclude globs (`*.key`, `target/**`, …) | yes, same core matcher | yes |
| Empty selection means nothing | yes | yes |
| Full dump vs capped preview | 32 KiB on screen, as `pulp ui`; `artifact_chunks` draws the full dump from the stored result (a Blob past 256 MiB, which Download saves and Copy declines) | `GET /api/artifact/{id}` |
| PDF / Office / zip extract | WASM in a nested extractor worker; a PDF or Office parser gets 30 s, whether the name or the leading bytes gave the kind | child process + 30 s timeout |
| A parser that panics, traps, runs out of stack, or hangs | its extractor worker is replaced; the file gets an `error` note and the pack goes on | the child fails; the file gets an `error` note, worded as the browser words it (`extractor panicked: …` for a panic) |
| A file edited after it is chosen | read as it is now: a pulp or preview opens each file afresh through the picker's handle or the drop's entry | read as it is now |
| A file that cannot be read at all: deleted, moved, its permission gone | flagged `changed`; a picked or dropped file that is gone is noted `[changed since scan: no longer there]`, as `pulp ui` notes it. The folder input's files are snapshots, so Chrome and Safari flag an edited one this way too; Firefox reads it as it is now | flagged `changed` |
| Pulp again | files whose name, size, modification time, and extraction settings match the dump on screen are taken from it, neither read nor extracted; the dump, stats, and outcomes match a fresh pulp. Only files the browser vouches for qualify: from the picker, and dropped in Chromium (see Known differences) | its own extraction cache |
| Damaged or encrypted files | flagged `unreadable`; the dump keeps a one-line note | same |
| Folders that cannot be read | a walk of a picked or dropped folder warns and names the first few; the folder input skips them | the scan warns and names the first few |
| Cooperative cancel | the pack worker stops the pack, terminating its extractor at once; the dump on screen stays | between files |
| Pack progress | files extracted | files extracted (`GET /api/progress`) |
| Byte budget (1 GiB) | spent in walk order at scan; the scan says when it stopped | same |
| Format or directory-map change after a pulp | redraws the stored result (`render_result`) in the worker that holds it | redraws the stored result |
| File preview | extracted text (`preview_file`, stores nothing) | extracted text (`POST /api/preview`, one file from the last scan; runs beside a pack) |
| Results kept in memory | the dump on screen, in the pack worker; a newer pulp takes its unchanged files and drops the rest, and choosing another folder frees the worker once a Download or Copy reading it is done | extracted files of recent pulps, bounded by count and text size (`src/store.rs`) |
| Try a sample | `/mill/sample.json`, in memory | `POST /api/sample`, written to a temp folder |

## Known differences

- **Names Chromium's folder picker leaves out.** The browser's folder picker hands over a folder through the File System Access API, which silently leaves out files and folders whose names Windows cannot hold: a `\`, `:`, `*`, `?`, `"`, `<`, `>`, or `|`, a trailing dot or space, `CON`, `NUL`, and the like. A dropped folder and the folder input list them.
- **A `\` in a folder name, with the folder input.** The input reports each path with `/` between its parts and gives no other source for folder names, so a `\` in a folder name arrives as `/`: `a\b/c.txt` is listed as `a/b/c.txt`, and the map shows `a/` holding `b/` where `pulp ui` shows `a\b/`. A `\` in a file name stays, and a dropped folder keeps both.
- **Where Pulp again reads everything.** A file is taken from the dump on screen only when the browser reports its size and modification time as they are now: files from the browser's pickers, and files dropped in Chromium. Elsewhere a size and time can be stale while the bytes are current (Firefox reports a picked file's size and time as it first saw them and reads its current bytes), so every pulp reads and extracts every ticked file. A file noted because the browser could not read it or its parser was lost (changed, timed out, crashed, not run) is always tried again.
- **A dismissed folder picker.** Chrome reports a closed picker, a declined permission prompt, and a folder it will not share with the same error. The mill treats each as a cancel and offers the folder input on the next Choose folder.
- **No workers.** Where the page cannot start workers, it packs on its own thread, where nothing could stop a parser that hangs. PDF, Office, EPUB, and RTF files, and archives when archives are on (their members may be any of those), are not parsed there: each gets an `error` note (`extractor not run: the page could not start a worker to parse this file in`), as a timeout is noted, the rest of the pack goes on, and a preview of one shows the same note.

## Layout

- Crate: `crates/pulp-wasm` (`wasm-bindgen`), artifacts in `site/mill/pkg/`.
- UI: `web/mill.js`, `web/mill.css`, and `web/pulp.css`, shared with `pulp ui`. `cargo xtask site` copies them (with the fonts, `web/sample.json`, and the site demo data) into `site/`; edit `web/`, not the copies.
- Browser shell and adapter: `site/mill/index.html`. A grant from the browser's pickers or a drop keeps the handles or entries it was given: each scan walks them, never opening the folders the filters leave out whole (the crate's `FolderFilter` decides, with the scan's own filters), and each pulp and preview opens every ticked file afresh. The folder input, the file input, and the sample hand over snapshots. Scans, trees, previews, and packs run in `site/mill/worker.js`; one pack worker per grant runs every pulp and keeps the dump on screen, and each pulp names that dump to `pack_begin` (`replaces`), which takes its unchanged files instead of extracting them again. A pack worker extracts through a nested worker of the same script, so a parser that panics or hangs costs only its file. Where workers cannot start (offline after the page loaded, a strict CSP), the page runs the same code from `worker.js` on its own thread, except the parsers that could hang (see Known differences). Asset paths are absolute (`/mill/...`) so `/mill` and `/mill/` both load.
- Shared extract/pack/render: `pulp` with `default-features = false`.

## Rebuild

```bash
wasm-pack build crates/pulp-wasm --target web --release --out-dir ../../site/mill/pkg
rm -f site/mill/pkg/.gitignore
cargo test -p pulp-wasm
```

`site/mill/worker.js` names the package it ships with in its `BUILD` line, the FNV-1a hash of `pkg/pulp_wasm_bg.wasm`. The page hands its compiled packer only to a worker of the same build, so a worker started after a later deploy loads its own packer instead of one its glue cannot link. After a rebuild, `cargo test -p pulp-wasm` fails until that line names the new package, and prints the line to write.

On macOS, `.cargo/config.toml` links host build scripts with the Command Line Tools clang, and its `-isysroot` flags do not apply when the target is wasm32. If the build fails while linking a build script, point the SDK at the Command Line Tools copy for that command: `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk wasm-pack build ...`.

After changing anything under `web/`, run `cargo xtask site` (or `cargo xtask site --check` in CI).

The result store, redraw, and preview logic in `crates/pulp-wasm` is plain Rust with native tests; `cargo xtask test` runs them with the rest of the workspace.

Serve `site/` locally. Vercel project **pulp-landing** uses Root Directory `site`.

## Non-goals

- Hosting the axum mill on a public hostname.
- Uploading folders to a server for packing.
