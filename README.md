# pdfview

View, edit and print scanned PDFs in your terminal. Built in Rust with Ratatui,
native Poppler/Cairo rendering and the Kitty graphics protocol, for Kitty and
Ghostty on Linux.

- Search and run every action from F1, with shortcuts and Vim-style commands.
- Configure the Catppuccin-style theme and print defaults in TOML.
- Rotate, crop, mirror, delete, and reorder pages.
- Select multiple pages, append images or PDFs, and scan new pages.
- Undo and redo edits with a branching history tree.
- Save structural edits without rasterizing pages; optionally compress scans.
- Print the edited document, marked pages or ranges through CUPS.

## Install

You need Rust 1.88+ and Cargo, Poppler with GLib bindings, Cairo, `qpdf`, and `pdfinfo`
(from Poppler). Image import also requires ImageMagick 7 (`magick`).

On Arch Linux:

```bash
sudo pacman -S --needed rust poppler-glib cairo qpdf imagemagick cups
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
cargo install --path . --locked --root ~/.local
```

This installs `pdfview` into `~/.local/bin`; make sure that directory is on
your `PATH`. Printing needs the CUPS client tools and a configured printer.

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

F1 or `?` opens the action browser. Type to filter by name, shortcut, command or
feature; use Up/Down to choose and Enter to run. Commands that need arguments
open the command bar with the relevant text already filled in. Esc closes it.
The browser is also available as `:help`.

| Keys | Action |
| --- | --- |
| `n` / `l` / Right / PageDown | Next page |
| `p` / `h` / Left / PageUp | Previous page |
| `g` / Home, `G` / End | First / last page |
| `o` | Enter a page number |
| `m` / Space | Mark or unmark the visible page |
| `v`, `V` | Select / unselect a range |
| `a`, `A` | Select all / clear selection |
| `r`, `R`, `M` | Rotate right / left, mirror |
| `d` / Delete | Delete selected/current pages |
| `c` | Crop selected/current pages |
| `<`, `>`, `J` | Move backward, forward, or before a target page |
| `z n`, `z l`, `z s` | Normal, light, strong compression |
| `i`, `O`, `S` | Append paths, browse files, scan |
| `u` / Ctrl+Z, `U` / Ctrl+R | Undo / redo |
| `t`, `H` | Browse history, enter a history node number |
| `s`, `w`, `N` | Save, save as, rename/move output |
| Ctrl+P / `P` | Print settings; Enter submits |
| Alt+P | Quick print with defaults |
| F5 | Reload configuration |
| `:` | Command bar |
| `q` / Esc, `Z Z`, `Z Q` | Quit, save and quit, discard and quit |

The command bar supports cursor movement, Home/End, Ctrl+A/E, Ctrl+U/K,
Backspace/Delete, Up/Down history, and Tab completion for unambiguous command
names. F1 can be opened while entering a command. Esc cancels input.
Action shortcuts also work with RU/UA layouts. Text entered into the command
bar, action search and filename fields remains unchanged.

Examples:

```text
:42
:select 1,3-5
:unselect 4-7
:rotate-left
:move 6
:compress light
:add 'scan one.jpg' other.pdf
:ls ~/Downloads
:save 'cleaned scan' ~/Documents
:rename 'new name'
:goto 3
:wq
:q!
```

`:q` keeps a document with unsaved changes open. Use `:wq` to save before
quitting or `:q!` to discard edits. The same applies to the corresponding keys.
Most edits apply to marked pages, or the visible page if nothing is marked.

In the file picker, Up/Down or j/k moves, Space marks files and Enter opens a
directory or appends marked files. In history, Enter restores the selected node.
Esc returns to the document. Both views scroll with the selection.

Crop mode uses arrows/hjkl to adjust the corresponding edge by 5 pixels,
HJKL by 50, Tab to choose an edge, and minus/underscore to reduce the active
margin. Enter applies and Esc cancels. n/p previews selected pages.

## Printing

Ctrl+P followed by Enter submits with the current defaults. Printer settings
load in the background; if still loading, Enter queues submission until they
are ready and Esc cancels it. Tab or
Up/Down chooses a field; Left/Right changes it. Space edits printer, page range,
copies or paper size. Fields also have letter shortcuts shown in the dialog.
While editing, Enter accepts the value, Esc cancels the edit and Ctrl+U clears it.

The dialog includes printer, pages, copies, monochrome/colour, orientation,
one-sided/duplex, paper size/type, quality and scaling. The default is to honour
the existing CUPS settings. PPD colour/quality values are used when advertised;
otherwise standard IPP options are used. Printers without advertised duplex
support do not offer it as an automatic feature.

Marked pages are printed by default when a selection exists; otherwise all pages
are printed. Unsaved edits are included. Page ranges refer to the document in
its current edited order. Use all, current, selected, or a range such as 1,3-5.

```text
:print
:print pages=all color=monochrome copies=2
:print printer=DeskJet_3630 paper=A4 quality=draft
:print! pages=current
```

`:print` opens settings; `:print!` and Alt+P submit directly. CUPS returns a job ID
in the status line. Errors keep the document open and the print dialog editable.
Use `lpstat -p -d` to inspect configured printers and `cancel JOB-ID` to cancel a
queued job.

## Configuration

The default theme is Catppuccin Mocha. The optional configuration file is
`$XDG_CONFIG_HOME/pdfview/config.toml`, or `~/.config/pdfview/config.toml`.
Create it from the supplied example:

```sh
mkdir -p ~/.config/pdfview
cp config.example.toml ~/.config/pdfview/config.toml
```

Missing keys keep their defaults. Invalid colours, options and unknown keys are
reported with the configuration path. F5 or `:reload` reloads the theme and print
defaults; a failed reload leaves the previous configuration intact. `:config`
shows the path. The example lists all theme colours and print settings.

## Notes

Rotate, delete, move, append, crop, and save are structural PDF operations
through `qpdf`. Mirror and compress rasterize affected pages. Compression
presets are lossy: `strong` is smallest, `normal` is balanced, and `light`
keeps the most detail. Legacy preset names `preview`, `email`, `archive`,
`screen`, `medium`, and `print` still work as aliases.

## Scanning

Press `S` to scan a page, or use `:scan 600` to override the configured DPI.
Scanning runs in the background with progress in the viewer. Esc cancels the
scan and its helper processes. Errors stay in the dialog; Enter or `r` retries,
and Esc returns to the document. No password prompt is opened.

A successful page is added immediately and has its own undo step. The scan
dialog stays open: place the next sheet and press Enter or `S`; Esc finishes
the series and returns to the document. A later failure/cancellation keeps
earlier pages. The last successful device is remembered for two minutes after
each page, including when reopening the scan dialog, so subsequent pages skip
device discovery. After a scan error, retry performs discovery again unless a
device URI is explicitly configured. This caches the device address; it does
not hold the hardware open or override the scanner's sleep/calibration policy.
Nothing is written over your input or output file until you save.

Start directly from the shell with:

```sh
pdfview --scan --out scan.pdf
pdfview --scan --scan-dpi 600 --out scan.pdf
```

The native path uses SANE's `scanimage` (with PDF output support). On Arch,
install `sane` and the backend for your scanner, for example `hplip` for an HP
DeskJet. A single connected scanner is detected automatically; virtual cameras
are ignored. If more than one scanner is connected, set its SANE device URI in
`[scanning]` in the config. `scanimage -f '%d%n'` lists those URIs.

The config also supports DPI, the backend's mode name (usually `Color`, `Gray`
or `Lineart`), an optional source and an overall timeout. F5 reloads it.
USB access comes from the distribution's scanner/udev setup; pdfview does not
run `sudo`, unload kernel modules or change device permissions. For access
errors, check that the scanner works with `scanimage` as your normal user.

For another backend, `PDFVIEW_SCAN_COMMAND` remains an optional noninteractive
shell command (`sh -c`). It receives the temporary output PDF path as `$1` and
DPI as `$2`. It must create a complete PDF and exit; stdin is closed, and the
same cancellation/timeout applies. The old personal `hpscan` script is not
required.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests create their own temporary documents. Tests that need external PDF tools
or `magick` return early when those tools are missing. CI runs on Arch Linux
with the PDF tools and ImageMagick 7 installed.

## License

[MIT](LICENSE)
