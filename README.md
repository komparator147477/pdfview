# pdfview

View and clean up scanned PDFs in your terminal. Built in Rust with native
Poppler/Cairo rendering and the Kitty graphics protocol, for Kitty and Ghostty
on Linux.

- Rotate, crop, mirror, delete, and reorder pages.
- Select multiple pages, append images or PDFs, and scan new pages.
- Undo and redo edits with a branching history tree.
- Save structural edits without rasterizing pages; optionally compress scans.

## Install

You need Rust and Cargo, Poppler with GLib bindings, Cairo, `qpdf`, and `pdfinfo`
(from Poppler). Image import also requires ImageMagick 7 (`magick`).

On Arch Linux:

```bash
sudo pacman -S --needed rust poppler-glib cairo qpdf imagemagick
```

On Debian/Ubuntu, install Rust and Cargo and the native build dependencies:

```bash
sudo apt install build-essential libpoppler-glib-dev libcairo2-dev qpdf poppler-utils
```

For image import, install ImageMagick 7 separately if your distribution only
provides ImageMagick 6 (`convert`).

Then build and install from the repository:

```bash
git clone https://github.com/komparator147477/pdfview.git
cd pdfview
cargo install --path . --locked
```

Cargo installs `pdfview` into `~/.cargo/bin`; make sure that directory is on
your `PATH`.

## Usage

```bash
pdfview file.pdf
pdfview
pdfview file.pdf --page 3
pdfview file.pdf --out cleaned.pdf
pdfview file.pdf --overwrite
```

By default, saving `file.pdf` writes `file-edited.pdf` beside the input.
Use `--out` to choose another path or `--overwrite` to save to the input path.
Edits are kept in a temporary document until you save.

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
Saved PDFs are rewritten through `qpdf` with a minimum PDF version of 1.7.

## Notes

Rotate, delete, move, append, crop, and save are structural PDF operations
through `qpdf`. Mirror and compress rasterize affected pages. Compression
presets are lossy: `strong` is smallest, `normal` is balanced, and `light`
keeps the most detail. Legacy preset names `preview`, `email`, `archive`,
`screen`, `medium`, and `print` still work as aliases.

## Scanning

Scanning requires `zsh`, `sudo`, and a command that produces a PDF. Set
`PDFVIEW_SCAN_COMMAND` to the command for your scanner. It receives the output
PDF path as `$1` and optional DPI as `$2` (an empty string when omitted):

```bash
export PDFVIEW_SCAN_COMMAND='/path/to/scan-to-pdf "$1" "$2"'
```

The command runs through `zsh -lc`. Before scanning, pdfview runs `sudo -v`
and may prompt for your password, even if your scan command does not use sudo.

The default is a personal `hpscan` integration and requires a separate local
script; configure the environment variable for your own setup:

```bash
source ~/coding/hpscan/hpscan.zsh && hpscan "$1" "$2"
```

## Development

```bash
cargo fmt --check
cargo test --locked
cargo build --release --locked
```

Tests create their own temporary documents. Tests that need external PDF tools
or `magick` return early when those tools are missing. CI runs on Arch Linux
with the PDF tools and ImageMagick 7 installed.

## License

[MIT](LICENSE)
