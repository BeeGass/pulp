# Known limitations

Open issues and fragile spots, as of the September 2026 overhaul. Remove a line when it is fixed.

- **CJK PDFs without a ToUnicode map** (Japanese from macOS Print to PDF, for one) lose their text: `pdf-extract` cannot map those fonts' codes. Predefined Unicode CMaps are patched in (`vendor/pdf-extract`); other CMaps are not.
- **No OCR.** A scanned PDF comes out empty.
- **Windows** has code paths (the Explorer picker, path handling) but no CI job and no manual test.
- **Chromium's folder picker** silently leaves out names Windows cannot hold (`\`, `:`, `*`, trailing dots, `CON`, and the like). Dropped folders and the folder input keep them.
- **The folder input reports `\` in a folder name as `/`,** so the browser mill's map can differ from `pulp ui`'s for such names. Known and documented in [docs/wasm-mill.md](../../docs/wasm-mill.md#known-differences).
- **Reuse on a re-pulp in the browser** applies only to files from the pickers and to Chromium drops; elsewhere a size and time can be stale while the bytes are current, so every file is read again.
- **RTF stays on the isolation list,** which costs a child process per file. The local end-to-end test's slow folder (`xtask/src/uitest/mill.rs`) is made of RTF files and depends on that cost; taking RTF off the list needs a new slow fixture.
- **The HTML nesting pre-scan** (`src/extract/html.rs`) mirrors html5ever 0.39's tokenizer to decide which pages are safe to lay out in process. An html5ever or html2text upgrade needs that model re-verified against the new parser.
- **The summary says "1 files"** and "listed 1 files": the counts are not pluralized, and tests pin that wording.
