# pdfview

Terminal PDF inspection and cleanup for scanned documents.

```bash
pdfview file.pdf
pdfview
pdfview file.pdf --page 3
pdfview file.pdf --out cleaned.pdf
pdfview file.pdf --overwrite
```

This version is a Rust CLI with native Poppler/Cairo page rendering and direct
Kitty/Ghostty image output. Required tools: `qpdf` and `pdfinfo`. Optional
workflow tools: `magick` for image import and `PDFVIEW_SCAN_COMMAND` for
scanner integration.

Build and install locally:

```bash
cargo build --release
~/.zshrc: pdfview() { ~/coding/pdfview/target/release/pdfview "$@"; }
```

## Controls

Move:
- `n` / right arrow / space: next page
- `p` / left arrow: previous page
- mouse wheel down/up: next/previous page
- `g`: first page
- `G`: last page
- `o`: prompt for a page number
- `:page N`: jump to page `N`

Select:
- `m`: mark or unmark the visible page
- `:select 1,3-5`: replace the current selection with pages 1, 3, 4, 5
- `:select all`: select every page
- `:unselect 4-7`: remove pages 4 through 7 from the selection
- `:unselect all`: clear the selection
- `:clear`: clear the selection

Edit:
- `r`: rotate the visible page right
- `R`: rotate the visible page left
- `M`: mirror the visible page
- `d`: delete the visible page
- `:move N`: move the selected pages, or the visible page if nothing is selected, before page `N`
- `c`: crop the visible page
- `:compress screen|medium|print`: raster-compress selected pages, or the visible page

Add:
- `S` or `:scan [DPI]`: scan pages with `PDFVIEW_SCAN_COMMAND`
- `:add image.jpg file.pdf`: append images or PDFs

History:
- `u` or `Ctrl-Z`: undo one edit
- `U`: redo into the newest child edit
- `:tree` or `:history`: show or hide the full history tree
- `:goto N`: jump to a history node by number

Help:
- `:help`: show command and keybinding reference in the page area

File:
- `s`: save
- `q`: quit

Running `pdfview` with no file starts an empty document. Use `S` or `:scan`
to create a multi-page scan, or `:add` to append image/PDF pages.

## Notes

Rotate, delete, move, append, and save are structural PDF operations through
`qpdf`. Crop and mirror rasterize affected pages, which matches the scanned
document cleanup use case.

Scanning runs a shell command that receives the output PDF path as `$1` and
optional DPI as `$2`.
Override it with `PDFVIEW_SCAN_COMMAND`; by default pdfview runs:

```bash
source ~/coding/hpscan/hpscan.zsh && hpscan "$1" "$2"
```
