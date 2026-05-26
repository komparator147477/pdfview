# pdfview

Terminal PDF inspection and cleanup for scanned documents.

```bash
pdfview file.pdf
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

Keys: `n/p` or arrows to navigate, `g/G` first/last, `+/-/0` zoom,
`m` mark page, `:select 1,3-5`, `:clear`, `r/R` rotate, `M` mirror,
`d` delete, `:move N`, `c` crop, `s` save, `q` quit.
