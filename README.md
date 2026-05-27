# pdfview

Terminal PDF inspection and cleanup for scanned documents.

```bash
pdfview file.pdf
pdfview
pdfview file.pdf --page 3
pdfview file.pdf --out cleaned.pdf
pdfview file.pdf --overwrite
```

Requires `uv`, `PyMuPDF` through the script metadata, and `chafa` on the system.
Install through `~/.zshrc`:

```zsh
pdfview() {
    uv run --script ~/coding/pdfview/pdfview "$@"
}
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
- `:clear`: clear the selection

Edit:
- `r`: rotate the visible page right
- `R`: rotate the visible page left
- `M`: mirror the visible page
- `d`: delete the visible page
- `:move N`: move the selected pages, or the visible page if nothing is selected, before page `N`
- `c`: crop the visible page

Add:
- `S` or `:scan`: scan pages with `hp-scan`
- `:add image.jpg file.pdf`: append images or PDFs

History:
- `u` or `Ctrl-Z`: undo one edit
- `U`: redo into the newest child edit
- `:history`: show or hide the compact history tree
- `:goto N`: jump to a history node by number

File:
- `s`: save
- `q`: quit

Running `pdfview` with no file starts an empty document. Use `S` or `:scan`
to create a multi-page scan, or `:add` to append image/PDF pages. Image pages
are placed on A4 with their aspect ratio preserved.

## Selection

Most single-key edits work on the visible page, so pressing `R` on page 2 edits
page 2 even if another page is selected. Selection is mainly for commands that
naturally work on a group, such as `:move N`.

Use `m` when you want to mark pages while browsing. Use `:select 1,3-5` when
you already know the page numbers. The header always shows how many pages are
selected.

## Crop

Press `c` to enter crop mode for the visible page.

- `h`: move the left crop edge inward
- `l`: move the right crop edge inward
- `k`: move the top crop edge inward
- `j`: move the bottom crop edge inward
- Uppercase `H/L/K/J`: larger steps
- `Tab`: cycle the active edge shown in the status line
- `-`: reduce the active edge margin
- `Enter`: apply the crop
- `Esc` or `q`: cancel

Cropping changes the PDF crop box. It does not rasterize the page.
