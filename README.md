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
- `d`: delete the visible page; deleting the last page returns to an empty document
- `:move N`: move the selected pages, or the visible page if nothing is selected, before page `N`
- `<` / `>` or `:back` / `:forward`: move selected pages, or the visible page, one slot
- `c`: crop the visible page
- `:compress light|normal|strong`: lossy raster-compress selected pages, or the visible page

Add:
- `S` or `:scan [DPI]`: scan pages with `PDFVIEW_SCAN_COMMAND`
- `:add`: open the add picker in the current directory
- `:add DIR`: open the add picker in `DIR`
- `:add image.jpg file.pdf`: append images or PDFs directly
- `:ls [DIR]`: open the add picker; `Space` marks multiple files, `Enter` adds marked files

History:
- `u` or `Ctrl-Z`: undo one edit
- `U`: redo into the newest child edit
- `:tree` or `:history`: show or hide the full history tree; arrows move, `Enter` restores
- `:goto N`: jump to a history node by number

Help:
- `:help`: show command and keybinding reference in the page area

File:
- `s` or `:save` / `:w`: save to the current output path
- `:save examplename`: save as `examplename.pdf` in the current output directory
- `:save PATH`: save to a new path and make it the current output; `.pdf` is added if omitted
- `:save NAME DIR`: save as `DIR/NAME.pdf`, for example `:save 'Mieterhohung 2026' ~/Desktop`
- `:rename NAME [DIR]`: rename or move the current output file/target
- `:save-as PATH` or `:write PATH`: legacy aliases for `:save PATH`
- `q` or `:q` / `:quit` / `:exit`: quit

Running `pdfview` with no file starts an empty document. Use `S` or `:scan`
to create a multi-page scan, or `:add` to append image/PDF pages.
The header shows `*` when there are unsaved changes and `save -> PATH` for
the current save target. That target may not exist yet until you save.

## Notes

Rotate, delete, move, append, crop, and save are structural PDF operations
through `qpdf`. Mirror and compress rasterize affected pages. Compression
presets are lossy: `strong` is smallest, `normal` is balanced, and `light`
keeps the most detail. Legacy preset names `preview`, `email`, `archive`,
`screen`, `medium`, and `print` still work as aliases.

Scanning runs a shell command that receives the output PDF path as `$1` and
optional DPI as `$2`.
Override it with `PDFVIEW_SCAN_COMMAND`; by default pdfview runs:

```bash
source ~/coding/hpscan/hpscan.zsh && hpscan "$1" "$2"
```
