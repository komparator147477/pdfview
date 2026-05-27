# Repository Guidelines

## Project Structure & Module Organization

This repository is a single Rust binary crate for a terminal PDF viewer/editor.

- `src/main.rs` contains the CLI, terminal UI, PDF rendering/editing logic, and unit tests.
- `Cargo.toml` declares Rust dependencies and crate metadata.
- `Cargo.lock` is committed for reproducible binary builds.
- `README.md` documents user-facing controls and setup.
- `target/` contains local build output and must not be committed.

There are no separate asset directories; PDF rendering uses system libraries/tools at runtime.

## Build, Test, and Development Commands

- `cargo test` runs unit tests in `src/main.rs`.
- `cargo build` builds a debug binary for local iteration.
- `cargo build --release` builds the optimized binary used by the shell wrapper.
- `./target/release/pdfview path/to/file.pdf` runs the release build directly.
- `rg "pattern" src README.md` is the preferred way to search the codebase.

Runtime expectations include `qpdf`, `pdfinfo`, Poppler/Cairo libraries, and a Kitty/Ghostty-compatible terminal for image display.

## Coding Style & Naming Conventions

Use idiomatic Rust 2021. Keep functions focused and prefer explicit `Result<T>` error handling with `anyhow::Context` where it improves diagnostics. Use `snake_case` for functions and variables, `CamelCase` for structs/enums, and descriptive names for user-visible operations such as `crop_pages`, `move_pages_by`, and `render_page_output`.

Before submitting changes, run:

```bash
cargo fmt
cargo test
```

Keep comments brief and only add them for non-obvious terminal, PDF, or FFI behavior.

## Testing Guidelines

Tests live in the `#[cfg(test)]` module in `src/main.rs`. Add focused unit tests for parsing, page range formatting, command helpers, and PDF structural operations. Tests that depend on external tools should skip gracefully when tools such as `qpdf`, `pdfinfo`, or `magick` are unavailable.

Use descriptive test names, for example `parses_reversed_ranges` or `command_backed_pdf_helpers_work`.

## Commit & Pull Request Guidelines

Commit history uses short imperative subjects, for example `Stabilize pdfview terminal redraw`. Follow that style:

```text
Add crop preview overlay
Fix qpdf append ranges
```

Pull requests should include a concise summary, commands run, and notes about terminal/PDF behavior tested manually. Include screenshots or terminal captures for UI changes, especially footer, prompt, crop, tree, help, or image rendering changes.

## Configuration Notes

Scanning is configured through `PDFVIEW_SCAN_COMMAND`; the command receives the output PDF path as `$1` and optional DPI as `$2`. Avoid hardcoding scanner-specific commands unless they are defaults with a documented override.
