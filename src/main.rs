mod actions;
mod config;
mod printing;
mod scanning;
mod ui;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{c_char, c_double, c_int, c_void, CString, OsStr};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::style::ResetColor;
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use serde_json::Value;
use tempfile::{Builder, TempDir};

const IMAGE_IMPORT_DPI: &str = "300";
const RASTER_EDIT_DPI: f64 = 300.0;
const SAVED_PDF_MIN_VERSION: &str = "1.7";
const IMAGE_EXTENSIONS: &[&str] = &[
    "bmp", "gif", "jfif", "jp2", "jpe", "jpeg", "jpg", "jpx", "pam", "pbm", "pgm", "png", "pnm",
    "ppm", "psd", "tif", "tiff", "webp",
];

const CAIRO_FORMAT_ARGB32: c_int = 0;

#[link(name = "poppler-glib")]
#[link(name = "gobject-2.0")]
extern "C" {
    fn poppler_document_new_from_file(
        uri: *const c_char,
        password: *const c_char,
        error: *mut *mut c_void,
    ) -> *mut c_void;
    fn poppler_document_get_page(document: *mut c_void, index: c_int) -> *mut c_void;
    fn poppler_page_get_size(page: *mut c_void, width: *mut c_double, height: *mut c_double);
    fn poppler_page_render(page: *mut c_void, cairo: *mut c_void);
    fn g_object_unref(object: *mut c_void);
}

#[link(name = "cairo")]
extern "C" {
    fn cairo_image_surface_create(format: c_int, width: c_int, height: c_int) -> *mut c_void;
    fn cairo_image_surface_create_for_data(
        data: *mut u8,
        format: c_int,
        width: c_int,
        height: c_int,
        stride: c_int,
    ) -> *mut c_void;
    fn cairo_image_surface_get_data(surface: *mut c_void) -> *mut u8;
    fn cairo_image_surface_get_stride(surface: *mut c_void) -> c_int;
    fn cairo_surface_flush(surface: *mut c_void);
    fn cairo_surface_destroy(surface: *mut c_void);
    fn cairo_create(surface: *mut c_void) -> *mut c_void;
    fn cairo_destroy(cairo: *mut c_void);
    fn cairo_scale(cairo: *mut c_void, sx: c_double, sy: c_double);
    fn cairo_set_source_rgb(cairo: *mut c_void, r: c_double, g: c_double, b: c_double);
    fn cairo_paint(cairo: *mut c_void);
    fn cairo_pdf_surface_create(
        filename: *const c_char,
        width: c_double,
        height: c_double,
    ) -> *mut c_void;
    fn cairo_set_source_surface(cairo: *mut c_void, surface: *mut c_void, x: c_double, y: c_double);
}

#[derive(Clone, Copy)]
struct PageSize {
    width: f64,
    height: f64,
}

#[derive(Clone, Copy)]
struct PageBox {
    left: f64,
    bottom: f64,
    right: f64,
    top: f64,
}

#[derive(Parser, Debug)]
#[command(version, about = "Terminal PDF viewer/editor for scanned PDF cleanup.")]
struct Args {
    pdf: Option<PathBuf>,
    #[arg(long, default_value_t = 1)]
    page: usize,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    overwrite: bool,
    #[arg(long, value_parser = ["kitty"])]
    format: Option<String>,
    /// Start scanning immediately, then review/save in the viewer.
    #[arg(long)]
    scan: bool,
    #[arg(long, requires = "scan")]
    scan_dpi: Option<u32>,
}

#[derive(Clone)]
struct Snapshot {
    parent: Option<usize>,
    label: String,
    pdf: Option<PathBuf>,
    page: usize,
    selected: BTreeSet<usize>,
    children: Vec<usize>,
}

struct State {
    original: Option<PathBuf>,
    current_pdf: Option<PathBuf>,
    out: PathBuf,
    overwrite: bool,
    page_count: usize,
    page: usize,
    selected: BTreeSet<usize>,
    dirty: bool,
    message: String,
    history: Vec<String>,
    snapshots: BTreeMap<usize, Snapshot>,
    current_snapshot: usize,
    saved_snapshot: usize,
    next_snapshot: usize,
    show_tree_page: bool,
    tree_cursor: usize,
    config: config::Config,
    ui: Option<ui::Runtime>,
    help: Option<actions::Help>,
    command: Option<actions::LineInput>,
    command_history: Vec<String>,
    command_history_cursor: Option<usize>,
    prefix: Option<char>,
    print_dialog: Option<printing::Dialog>,
    scan: Option<scanning::Session>,
    scan_device: Option<scanning::CachedDevice>,
    listing_page: Option<ListingPage>,
    crop_preview: Option<CropPreview>,
    render_cache: Arc<Mutex<HashMap<RenderKey, Vec<u8>>>>,
    pending_renders: Arc<Mutex<HashSet<RenderKey>>>,
    last_rendered: Option<Vec<u8>>,
    last_rendered_key: Option<RenderKey>,
    temp: TempDir,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RenderKey {
    snapshot: usize,
    page: usize,
    cols: u16,
    rows: u16,
    format: String,
}

#[derive(Clone)]
struct RenderRequest {
    key: RenderKey,
    pdf: PathBuf,
    page: usize,
    cols: u16,
    rows: u16,
    format: String,
}

struct ListingPage {
    title: String,
    entries: Vec<ListingEntry>,
    selected: usize,
    marked: BTreeSet<usize>,
}

struct ListingEntry {
    label: String,
    path: PathBuf,
    kind: ListingEntryKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ListingEntryKind {
    Directory,
    File,
    Empty,
}

impl State {
    fn new(args: Args, temp: TempDir) -> Result<Self> {
        let original = match args.pdf {
            Some(path) => {
                let path = expand_path(path)?;
                ensure_file(&path)?;
                Some(path)
            }
            None => None,
        };
        let out = if args.overwrite {
            original
                .clone()
                .ok_or_else(|| anyhow!("--overwrite requires an input PDF"))?
        } else if let Some(out) = args.out {
            pdf_output_path(expand_path(out)?)?
        } else if let Some(path) = &original {
            path.with_file_name(format!(
                "{}-edited.pdf",
                path.file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or("document")
            ))
        } else {
            std::env::current_dir()?.join("scan.pdf")
        };

        let current_pdf = if let Some(path) = &original {
            let copy = temp.path().join("current-0.pdf");
            fs::copy(path, &copy).with_context(|| format!("copy {}", path.display()))?;
            Some(copy)
        } else {
            None
        };
        let page_count = if let Some(pdf) = &current_pdf {
            page_count(pdf)?
        } else {
            0
        };
        if original.is_some() && page_count == 0 {
            bail!("empty PDF");
        }

        let mut state = Self {
            original,
            current_pdf,
            out,
            overwrite: args.overwrite,
            page_count,
            page: args.page.saturating_sub(1),
            selected: BTreeSet::new(),
            dirty: false,
            message: String::new(),
            history: Vec::new(),
            snapshots: BTreeMap::new(),
            current_snapshot: 0,
            saved_snapshot: 0,
            next_snapshot: 1,
            show_tree_page: false,
            tree_cursor: 0,
            config: config::Config::default(),
            ui: None,
            help: None,
            command: None,
            command_history: Vec::new(),
            command_history_cursor: None,
            prefix: None,
            print_dialog: None,
            scan: None,
            scan_device: None,
            listing_page: None,
            crop_preview: None,
            render_cache: Arc::new(Mutex::new(HashMap::new())),
            pending_renders: Arc::new(Mutex::new(HashSet::new())),
            last_rendered: None,
            last_rendered_key: None,
            temp,
        };
        state.clamp_page();
        state.init_snapshots()?;
        Ok(state)
    }

    fn current_pdf(&self) -> Result<&Path> {
        self.current_pdf
            .as_deref()
            .ok_or_else(|| anyhow!("no PDF pages; use :add or S first"))
    }

    fn targets(&self, use_selection: bool) -> Result<Vec<usize>> {
        if self.page_count == 0 {
            bail!("no pages to edit");
        }
        let pages = if use_selection && !self.selected.is_empty() {
            self.selected.iter().copied().collect()
        } else {
            vec![self.page]
        };
        Ok(pages)
    }

    fn clamp_page(&mut self) {
        if self.page_count == 0 {
            self.page = 0;
            self.selected.clear();
        } else {
            self.page = self.page.min(self.page_count - 1);
            self.selected.retain(|p| *p < self.page_count);
        }
    }

    fn add_history(&mut self, message: impl Into<String>) {
        self.history.push(message.into());
        if self.history.len() > 3 {
            self.history.remove(0);
        }
    }

    fn init_snapshots(&mut self) -> Result<()> {
        let pdf = self.copy_snapshot_pdf(0)?;
        self.snapshots.insert(
            0,
            Snapshot {
                parent: None,
                label: if self.original.is_some() {
                    "opened".into()
                } else {
                    "empty".into()
                },
                pdf,
                page: self.page,
                selected: self.selected.clone(),
                children: Vec::new(),
            },
        );
        Ok(())
    }

    fn copy_snapshot_pdf(&self, id: usize) -> Result<Option<PathBuf>> {
        let Some(current) = &self.current_pdf else {
            return Ok(None);
        };
        let path = self.temp.path().join(format!("snapshot-{id}.pdf"));
        fs::copy(current, &path).with_context(|| format!("copy snapshot {}", current.display()))?;
        Ok(Some(path))
    }

    fn replace_current(&mut self, pdf: PathBuf, label: impl Into<String>) -> Result<()> {
        qpdf_check(&pdf)?;
        self.page_count = page_count(&pdf)?;
        if self.page_count == 0 {
            bail!("operation produced an empty PDF");
        }
        self.current_pdf = Some(pdf);
        self.clear_render_cache();
        self.clamp_page();
        self.record_snapshot(label)
    }

    fn record_snapshot(&mut self, label: impl Into<String>) -> Result<()> {
        let label = label.into();
        self.dirty = true;
        self.add_history(label.clone());
        let id = self.next_snapshot;
        self.next_snapshot += 1;
        let pdf = self.copy_snapshot_pdf(id)?;
        self.snapshots.insert(
            id,
            Snapshot {
                parent: Some(self.current_snapshot),
                label,
                pdf,
                page: self.page,
                selected: self.selected.clone(),
                children: Vec::new(),
            },
        );
        self.snapshots
            .get_mut(&self.current_snapshot)
            .expect("snapshot exists")
            .children
            .push(id);
        self.current_snapshot = id;
        Ok(())
    }

    fn restore_snapshot(&mut self, id: usize) -> Result<()> {
        let node = self
            .snapshots
            .get(&id)
            .ok_or_else(|| anyhow!("history node not found: {id}"))?
            .clone();
        self.current_pdf = if let Some(pdf) = node.pdf {
            let restored = self.temp.path().join(format!("restore-{id}.pdf"));
            fs::copy(&pdf, &restored)?;
            Some(restored)
        } else {
            None
        };
        self.page_count = if let Some(pdf) = &self.current_pdf {
            page_count(pdf)?
        } else {
            0
        };
        self.page = node.page;
        self.selected = node.selected;
        self.current_snapshot = id;
        self.clear_render_cache();
        self.dirty = self.current_snapshot != self.saved_snapshot;
        self.show_tree_page = true;
        self.clamp_page();
        Ok(())
    }

    fn clear_render_cache(&mut self) {
        if let Ok(mut cache) = self.render_cache.lock() {
            cache.clear();
        }
        if let Ok(mut pending) = self.pending_renders.lock() {
            pending.clear();
        }
        self.last_rendered_key = None;
        self.last_rendered = None;
    }
}

#[derive(Clone, Copy)]
struct CropPreview {
    margins: [u32; 4],
    active_edge: usize,
}

#[derive(Clone, Copy)]
enum Input {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Esc,
    CtrlC,
    CtrlZ,
    Left,
    Right,
    Up,
    Down,
    WheelUp,
    WheelDown,
    Backspace,
    Ctrl(char),
    Alt(char),
    Function(u8),
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Resize,
    Unknown,
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            terminal::enable_raw_mode()?;
            let guard = Self { active: true };
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                Hide,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )?;
            Ok(guard)
        } else {
            Ok(Self { active: false })
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(
                io::stdout(),
                Clear(ClearType::All),
                DisableMouseCapture,
                Show,
                ResetColor,
                PopKeyboardEnhancementFlags,
                LeaveAlternateScreen
            );
            let _ = io::stdout()
                .write_all(b"\x1b[?2026l\x1b_Ga=d,d=A,q=2;\x1b\\\x1b_Ga=d,d=a,q=2;\x1b\\");
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    require_tool("qpdf")?;
    require_tool("pdfinfo")?;

    let fmt = args.format.clone().unwrap_or_else(native_format);
    let scan = args.scan;
    let scan_dpi = args.scan_dpi;
    let temp = tempfile::tempdir()?;
    let mut state = State::new(args, temp)?;
    state.config = config::Config::load()?;
    if scan {
        scan_pages(&mut state, scan_dpi)?;
    }
    run_ui(&mut state, &fmt)
}

fn run_ui(state: &mut State, fmt: &str) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut redraw = true;
    loop {
        if redraw {
            if let Err(err) = ui::show(state, fmt) {
                state.message = err.to_string();
            }
        }
        redraw = current_page_render_is_ready(state, fmt);
        if redraw {
            continue;
        }
        match handle_input(state, fmt, read_input()?)? {
            UiAction::Redraw => redraw = true,
            UiAction::Noop => redraw = false,
            UiAction::Quit => return Ok(()),
        }
    }
}

enum UiAction {
    Redraw,
    Noop,
    Quit,
}

fn execute_action(state: &mut State, fmt: &str, command: &str) -> Result<UiAction> {
    if command.ends_with(' ') {
        state.command = Some(actions::LineInput::new(command));
        state.command_history_cursor = None;
        return Ok(UiAction::Redraw);
    }
    match command_line(state, fmt, command) {
        Ok(action) => Ok(action),
        Err(error) => {
            state.message = error.to_string();
            Ok(UiAction::Redraw)
        }
    }
}

fn handle_input(state: &mut State, fmt: &str, input: Input) -> Result<UiAction> {
    if state.scan.is_some() {
        return handle_scan_input(state, input);
    }
    let printer_changed = state
        .print_dialog
        .as_mut()
        .is_some_and(printing::Dialog::poll);
    if matches!(input, Input::Unknown) && !printer_changed {
        return Ok(UiAction::Noop);
    }
    if matches!(input, Input::Resize) {
        state.clear_render_cache();
        if let Some(ui) = &mut state.ui {
            ui.invalidate();
        }
        return Ok(UiAction::Redraw);
    }
    if let Some(mut dialog) = state.print_dialog.take() {
        let input = if dialog.editing.is_none() {
            actions::normal_input(input)
        } else {
            input
        };
        let event = if matches!(input, Input::Unknown) && dialog.pending_submit && !dialog.loading()
        {
            printing::DialogEvent::Submit
        } else {
            dialog.input(input)
        };
        match event {
            printing::DialogEvent::Cancel => {}
            printing::DialogEvent::Continue => state.print_dialog = Some(dialog),
            printing::DialogEvent::Submit => {
                match print_document(state, &dialog.options, &dialog.caps) {
                    Ok(()) => {
                        state.config.printing = dialog.options;
                        state.config.printing.pages = "all".into();
                    }
                    Err(error) => {
                        dialog.error = error.to_string();
                        state.print_dialog = Some(dialog);
                    }
                }
            }
        }
        return Ok(UiAction::Redraw);
    }
    if let Some(mut help) = state.help.take() {
        let indices = actions::matching(&help.query.text);
        match input {
            Input::Esc | Input::CtrlC | Input::Function(1) => {}
            Input::Enter => {
                if let Some(&index) = indices.get(help.selected) {
                    state.command = None;
                    return execute_action(state, fmt, actions::ACTIONS[index].command);
                }
                state.help = Some(help);
            }
            _ => {
                match input {
                    Input::Up | Input::Ctrl('p') | Input::WheelUp => {
                        help.selected = help.selected.saturating_sub(1)
                    }
                    Input::Down | Input::Ctrl('n') | Input::WheelDown => {
                        help.selected = (help.selected + 1).min(indices.len().saturating_sub(1))
                    }
                    Input::PageUp => help.selected = help.selected.saturating_sub(10),
                    Input::PageDown => {
                        help.selected = (help.selected + 10).min(indices.len().saturating_sub(1))
                    }
                    _ => {
                        if help.query.edit(input) {
                            help.selected = 0;
                        }
                    }
                }
                state.help = Some(help);
            }
        }
        return Ok(UiAction::Redraw);
    }
    if let Some(mut command) = state.command.take() {
        match input {
            Input::Esc | Input::CtrlC => {}
            Input::Function(1) => {
                state.help = Some(actions::Help::default());
            }
            Input::Enter => {
                if !command.text.trim().is_empty() {
                    if state.command_history.last() != Some(&command.text) {
                        state.command_history.push(command.text.clone());
                    }
                    return execute_action(state, fmt, command.text.trim());
                }
            }
            _ => {
                match input {
                    Input::Tab => {
                        let matches: std::collections::BTreeSet<&str> = actions::ACTIONS
                            .iter()
                            .filter_map(|a| {
                                a.command
                                    .split_whitespace()
                                    .next()
                                    .filter(|name| name.starts_with(&command.text))
                            })
                            .collect();
                        if matches.len() == 1 {
                            command =
                                actions::LineInput::new(format!("{} ", matches.first().unwrap()));
                        }
                    }
                    Input::Up => {
                        let cursor = state
                            .command_history_cursor
                            .unwrap_or(state.command_history.len())
                            .saturating_sub(1);
                        if let Some(text) = state.command_history.get(cursor) {
                            command = actions::LineInput::new(text);
                            state.command_history_cursor = Some(cursor);
                        }
                    }
                    Input::Down => {
                        let cursor = (state
                            .command_history_cursor
                            .unwrap_or(state.command_history.len())
                            + 1)
                        .min(state.command_history.len());
                        command = actions::LineInput::new(
                            state
                                .command_history
                                .get(cursor)
                                .cloned()
                                .unwrap_or_default(),
                        );
                        state.command_history_cursor = Some(cursor);
                    }
                    _ => {
                        command.edit(input);
                    }
                }
                state.command = Some(command);
            }
        }
        return Ok(UiAction::Redraw);
    }
    let input = actions::normal_input(input);
    if let Some(prefix) = state.prefix.take() {
        let command = match (prefix, input) {
            ('z', Input::Char('n')) => Some("compress normal"),
            ('z', Input::Char('l')) => Some("compress light"),
            ('z', Input::Char('s')) => Some("compress strong"),
            ('Z', Input::Char('Z')) => Some("wq"),
            ('Z', Input::Char('Q')) => Some("q!"),
            _ => None,
        };
        if let Some(command) = command {
            return execute_action(state, fmt, command);
        }
        return Ok(UiAction::Redraw);
    }
    match input {
        Input::Char(':') => return prompt_command(state, fmt),
        Input::Function(1) | Input::Char('?') => return execute_action(state, fmt, "help"),
        Input::Ctrl('p') => return execute_action(state, fmt, "print"),
        Input::Alt('p') => return execute_action(state, fmt, "print!"),
        Input::Function(5) => return execute_action(state, fmt, "reload"),
        Input::CtrlC => return execute_action(state, fmt, "q"),
        _ => {}
    }
    if state.listing_page.is_some() {
        return handle_listing_input(state, fmt, input);
    }
    if state.show_tree_page {
        if matches!(input, Input::Char('t')) {
            state.show_tree_page = false;
            return Ok(UiAction::Redraw);
        }
        return handle_tree_input(state, fmt, input);
    }
    if matches!(input, Input::Char('z' | 'Z')) {
        if let Input::Char(prefix) = input {
            state.prefix = Some(prefix);
        }
        return Ok(UiAction::Redraw);
    }
    if let Some(command) = actions::binding(input) {
        state.message.clear();
        return execute_action(state, fmt, command);
    }
    Ok(UiAction::Noop)
}

fn print_pages(options: &printing::PrintOptions, state: &State) -> Result<Vec<usize>> {
    if state.page_count == 0 {
        bail!("no pages to print");
    }
    match options.pages.trim() {
        "all" => Ok((0..state.page_count).collect()),
        "current" => Ok(vec![state.page]),
        "selected" | "selection" => {
            if state.selected.is_empty() {
                bail!("no marked pages; choose all, current, or a page range");
            }
            Ok(state.selected.iter().copied().collect())
        }
        range => Ok(parse_pages(range, state.page_count)?.into_iter().collect()),
    }
}

fn print_document(
    state: &mut State,
    options: &printing::PrintOptions,
    caps: &printing::Capabilities,
) -> Result<()> {
    let pages = print_pages(options, state)?;
    let current = state.current_pdf()?.to_path_buf();
    let subset = state.temp.path().join("print-pages.pdf");
    let pdf = if pages == (0..state.page_count).collect::<Vec<_>>() {
        &current
    } else {
        rebuild_from_ranges(&current, &pages, &subset)?;
        &subset
    };
    let title = state
        .original
        .as_ref()
        .and_then(|p| p.file_name())
        .unwrap_or_else(|| OsStr::new("pdfview document"))
        .to_string_lossy();
    let result = printing::submit(options, caps, &title, pdf)?;
    state.message = format!("Print queued · {result}");
    Ok(())
}

fn handle_listing_input(state: &mut State, fmt: &str, input: Input) -> Result<UiAction> {
    match input {
        Input::Esc | Input::Char('q') => {
            state.listing_page = None;
            Ok(UiAction::Redraw)
        }
        Input::Up | Input::Char('k') | Input::WheelUp => {
            if let Some(listing) = &mut state.listing_page {
                listing.selected = listing.selected.saturating_sub(1);
            }
            Ok(UiAction::Redraw)
        }
        Input::Down | Input::Char('j') | Input::WheelDown => {
            if let Some(listing) = &mut state.listing_page {
                listing.selected =
                    (listing.selected + 1).min(listing.entries.len().saturating_sub(1));
            }
            Ok(UiAction::Redraw)
        }
        Input::Char(' ') | Input::Char('m') => {
            if let Some(listing) = &mut state.listing_page {
                if listing
                    .entries
                    .get(listing.selected)
                    .is_some_and(|entry| entry.kind == ListingEntryKind::File)
                    && !listing.marked.insert(listing.selected)
                {
                    listing.marked.remove(&listing.selected);
                }
            }
            Ok(UiAction::Redraw)
        }
        Input::Enter => {
            let result = activate_listing_selection(state);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char(':') => prompt_command(state, fmt),
        _ => Ok(UiAction::Noop),
    }
}

fn handle_tree_input(state: &mut State, fmt: &str, input: Input) -> Result<UiAction> {
    match input {
        Input::Esc | Input::Char('q') => {
            state.show_tree_page = false;
            Ok(UiAction::Redraw)
        }
        Input::Up | Input::Char('k') | Input::WheelUp => {
            state.tree_cursor = state.tree_cursor.saturating_sub(1);
            Ok(UiAction::Redraw)
        }
        Input::Down | Input::Char('j') | Input::WheelDown => {
            let max = tree_lines(state).len().saturating_sub(1);
            state.tree_cursor = (state.tree_cursor + 1).min(max);
            Ok(UiAction::Redraw)
        }
        Input::Enter => {
            let result = activate_tree_selection(state);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char(':') => prompt_command(state, fmt),
        _ => Ok(UiAction::Noop),
    }
}

fn current_page_render_is_ready(state: &State, fmt: &str) -> bool {
    if state.show_tree_page
        || state.help.is_some()
        || state.print_dialog.is_some()
        || state.scan.is_some()
        || state.listing_page.is_some()
    {
        return false;
    }
    let (cols, rows) = terminal::size().unwrap_or((100, 40));
    let area = ui::document_area(ratatui::layout::Rect::new(0, 0, cols, rows));
    let key = RenderKey {
        snapshot: state.current_snapshot,
        page: state.page,
        cols: area.width,
        rows: area.height,
        format: fmt.to_string(),
    };
    state.last_rendered_key.as_ref() != Some(&key)
        && state
            .render_cache
            .lock()
            .is_ok_and(|cache| cache.contains_key(&key))
}

fn handle_result(state: &mut State, result: Result<()>) {
    if let Err(err) = result {
        state.message = err.to_string();
    }
}

fn input_from_event(event: Event) -> Input {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Release => Input::Unknown,
        Event::Key(key) => match key.code {
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match actions::shortcut_char(c) {
                    'c' => Input::CtrlC,
                    'z' => Input::CtrlZ,
                    'p' if key.kind == KeyEventKind::Repeat => Input::Unknown,
                    key => Input::Ctrl(key),
                }
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => {
                let c = actions::shortcut_char(c);
                if c == 'p' && key.kind == KeyEventKind::Repeat {
                    Input::Unknown
                } else {
                    Input::Alt(c)
                }
            }
            KeyCode::Char(c) => Input::Char(c),
            KeyCode::F(number) => Input::Function(number),
            KeyCode::Enter => Input::Enter,
            KeyCode::Tab => Input::Tab,
            KeyCode::BackTab => Input::BackTab,
            KeyCode::Esc => Input::Esc,
            KeyCode::Left => Input::Left,
            KeyCode::Right => Input::Right,
            KeyCode::Up => Input::Up,
            KeyCode::Down => Input::Down,
            KeyCode::Backspace => Input::Backspace,
            KeyCode::Delete => Input::Delete,
            KeyCode::Home => Input::Home,
            KeyCode::End => Input::End,
            KeyCode::PageUp => Input::PageUp,
            KeyCode::PageDown => Input::PageDown,
            _ => Input::Unknown,
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollDown => Input::WheelDown,
            MouseEventKind::ScrollUp => Input::WheelUp,
            _ => Input::Unknown,
        },
        Event::Resize(_, _) => Input::Resize,
        _ => Input::Unknown,
    }
}

fn read_input() -> Result<Input> {
    if !io::stdin().is_terminal() {
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        return Ok(Input::Char('q'));
    }
    if event::poll(Duration::from_millis(80))? {
        Ok(input_from_event(event::read()?))
    } else {
        Ok(Input::Unknown)
    }
}

fn tree_lines(state: &State) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    build_tree_lines(state, 0, String::new(), true, &mut lines);
    lines
}

fn activate_tree_selection(state: &mut State) -> Result<()> {
    let lines = tree_lines(state);
    let Some((id, _)) = lines.get(state.tree_cursor) else {
        return Ok(());
    };
    state.restore_snapshot(*id)?;
    state.tree_cursor = tree_lines(state)
        .iter()
        .position(|(line_id, _)| *line_id == *id)
        .unwrap_or(0);
    state.message = format!("history: moved to {id}");
    state.add_history(state.message.clone());
    Ok(())
}

fn build_tree_lines(
    state: &State,
    id: usize,
    prefix: String,
    is_last: bool,
    lines: &mut Vec<(usize, String)>,
) {
    let Some(node) = state.snapshots.get(&id) else {
        return;
    };
    let marker = if id == state.current_snapshot {
        "* "
    } else if id == state.saved_snapshot {
        "S "
    } else {
        "  "
    };
    let branch = if node.parent.is_none() {
        ""
    } else if is_last {
        "`- "
    } else {
        "|- "
    };
    lines.push((id, format!("{marker}{prefix}{branch}{id} {}", node.label)));
    let child_prefix = if node.parent.is_none() {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}|  ")
    };
    for (idx, child) in node.children.iter().enumerate() {
        build_tree_lines(
            state,
            *child,
            child_prefix.clone(),
            idx + 1 == node.children.len(),
            lines,
        );
    }
}

fn begin_sync_update() -> Result<()> {
    io::stdout().write_all(b"\x1b[?2026h")?;
    Ok(())
}

fn clear_terminal_images(fmt: &str) -> Result<()> {
    if fmt == "kitty" {
        io::stdout().write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b_Ga=d,d=a,q=2;\x1b\\")?;
    }
    Ok(())
}

fn end_sync_update() -> Result<()> {
    io::stdout().write_all(b"\x1b[?2026l")?;
    io::stdout().flush()?;
    Ok(())
}

fn rendered_page_output(state: &mut State, fmt: &str, cols: u16, rows: u16) -> Result<Vec<u8>> {
    if let Some(preview) = state.crop_preview {
        let pdf = state.current_pdf()?.to_path_buf();
        return render_page_output_with_crop_preview(
            &pdf,
            state.page + 1,
            cols,
            rows,
            fmt,
            preview,
        );
    }
    let key = RenderKey {
        snapshot: state.current_snapshot,
        page: state.page,
        cols,
        rows,
        format: fmt.to_string(),
    };
    let changed = state.last_rendered_key.as_ref() != Some(&key);
    if changed {
        if let Ok(mut cache) = state.render_cache.lock() {
            cache.retain(|cached, _| {
                cached.snapshot == key.snapshot
                    && cached.cols == cols
                    && cached.rows == rows
                    && cached.page.abs_diff(state.page) <= 4
            });
        }
    }
    if let Some(output) = state
        .render_cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned())
    {
        if state.message.starts_with("rendering page ") {
            state.message.clear();
        }
        state.last_rendered = Some(output.clone());
        state.last_rendered_key = Some(key);
        if changed {
            prefetch_pages(state, cols, rows, fmt);
        }
        return Ok(output);
    }

    let pdf = state.current_pdf()?.to_path_buf();
    if let Some(previous) = &state.last_rendered {
        spawn_render_if_needed(
            Arc::clone(&state.render_cache),
            Arc::clone(&state.pending_renders),
            RenderRequest {
                key,
                pdf,
                page: state.page + 1,
                cols,
                rows,
                format: fmt.to_string(),
            },
        );
        state.message = format!("rendering page {}...", state.page + 1);
        return Ok(previous.clone());
    }

    let output = render_page_output(&pdf, state.page + 1, cols, rows, fmt)?;
    if let Ok(mut cache) = state.render_cache.lock() {
        cache.insert(key.clone(), output.clone());
    }
    state.last_rendered = Some(output.clone());
    state.last_rendered_key = Some(key);
    prefetch_pages(state, cols, rows, fmt);
    Ok(output)
}

fn confirm(state: &mut State, label: &str) -> Result<bool> {
    ui::confirm(state, label)
}

fn go_to_page(state: &mut State, raw: &str) -> Result<()> {
    let page: usize = raw.parse().context("page number required")?;
    if page == 0 || page > state.page_count {
        bail!("page out of range: 1-{}", state.page_count);
    }
    state.page = page - 1;
    state.add_history(format!("go to page {page}"));
    Ok(())
}

fn prompt_command(state: &mut State, _fmt: &str) -> Result<UiAction> {
    state.command = Some(actions::LineInput::default());
    state.command_history_cursor = None;
    Ok(UiAction::Redraw)
}

fn command_line(state: &mut State, fmt: &str, line: &str) -> Result<UiAction> {
    let parts = shell_words::split(line.trim()).context("parse command")?;
    if parts.is_empty() {
        return Ok(UiAction::Noop);
    }
    let name = parts[0].trim_start_matches(':');
    if name.chars().all(|c| c.is_ascii_digit()) && !name.is_empty() {
        go_to_page(state, name)?;
        return Ok(UiAction::Redraw);
    }
    match name {
        "next" | "prev" | "first" | "last" => {
            state.show_tree_page = false;
            state.listing_page = None;
            state.page = match name {
                "next" => (state.page + 1).min(state.page_count.saturating_sub(1)),
                "prev" => state.page.saturating_sub(1),
                "first" => 0,
                _ => state.page_count.saturating_sub(1),
            };
            Ok(())
        }
        "mark" => {
            if state.page_count == 0 {
                bail!("no pages to mark");
            }
            if !state.selected.insert(state.page) {
                state.selected.remove(&state.page);
            }
            Ok(())
        }
        "print" | "print!" => {
            if state.page_count == 0 {
                bail!("no pages to print");
            }
            let options = printing::options_from_args(
                &state.config.printing,
                &parts[1..],
                !state.selected.is_empty(),
            )?;
            if name == "print!" {
                let needs_caps = options.color != printing::ColorMode::Auto
                    || options.quality != printing::Quality::Auto
                    || options.paper_type != printing::PaperType::Auto
                    || options.paper.is_some()
                    || matches!(
                        options.sides,
                        printing::Sides::LongEdge | printing::Sides::ShortEdge
                    );
                let caps = if needs_caps {
                    printing::printer_info(options.printer.as_deref()).1
                } else {
                    printing::Capabilities::default()
                };
                print_document(state, &options, &caps)?;
            } else {
                state.print_dialog =
                    Some(printing::Dialog::open(options, !state.selected.is_empty()));
            }
            Ok(())
        }
        "reload" => {
            state.config = config::Config::load()?;
            if let Some(ui) = &mut state.ui {
                ui.invalidate();
            }
            state.message = "Theme and print defaults reloaded".into();
            Ok(())
        }
        "config" => {
            state.message = config::path().display().to_string();
            Ok(())
        }
        "page" if parts.len() == 2 => go_to_page(state, &parts[1]),
        "select" if parts.len() == 2 => {
            state.selected = if parts[1] == "all" {
                (0..state.page_count).collect()
            } else {
                parse_pages(&parts[1], state.page_count)?
            };
            state.add_history(format!(
                "select: page {}",
                compact_page_list_label(state.selected.iter().copied())
            ));
            Ok(())
        }
        "unselect" if parts.len() == 2 => {
            if parts[1] == "all" {
                state.selected.clear();
                state.add_history("clear selection");
            } else {
                let pages = parse_pages(&parts[1], state.page_count)?;
                for page in &pages {
                    state.selected.remove(page);
                }
                state.add_history(format!("unselect: page {}", compact_page_list_label(pages)));
            }
            Ok(())
        }
        "clear" => {
            state.selected.clear();
            state.add_history("clear selection");
            Ok(())
        }
        "save" | "w" => save_command(state, &parts[1..]),
        "save-as" | "write" => save_as_command(state, &parts[1..]),
        "rename" | "ren" => rename_command(state, &parts[1..]),
        "q" | "quit" | "exit" | "close" => {
            if state.dirty {
                bail!("Unsaved changes · :wq saves and quits · :q! discards edits");
            }
            return Ok(UiAction::Quit);
        }
        "q!" | "quit!" => return Ok(UiAction::Quit),
        "wq" | "x" => {
            save(state, None)?;
            if state.dirty {
                bail!("Save cancelled; document remains open");
            }
            return Ok(UiAction::Quit);
        }
        "undo" => undo(state),
        "redo" => redo(state),
        "rotate" | "rot" | "right" => rotate(state, 90, true),
        "rotate-left" | "rot-left" | "left" => rotate(state, -90, true),
        "delete" | "del" | "d" => delete_pages(state, true),
        "mirror" | "flip" => mirror(state, true),
        "crop" => crop_pages(state, fmt, true),
        "compress" => {
            let level = parts.get(1).map(String::as_str).unwrap_or("medium");
            compress_pages(state, level, true)
        }
        "move" if parts.len() == 2 => {
            let target = parts[1].parse::<usize>().context("target page required")?;
            move_pages(state, target)
        }
        "back" | "backward" | "<" => move_pages_by(state, -1),
        "forward" | ">" => move_pages_by(state, 1),
        "scan" => {
            let dpi = parts
                .get(1)
                .map(|raw| raw.parse::<u32>().context("scan dpi must be a number"))
                .transpose()?;
            scan_pages(state, dpi)
        }
        "add" => add_command(state, &parts[1..]),
        "la" | "ls" => list_addable_files(state, parts.get(1).map(String::as_str)),
        "history" | "tree" => {
            state.show_tree_page = !state.show_tree_page;
            state.help = None;
            state.listing_page = None;
            state.tree_cursor = tree_lines(state)
                .iter()
                .position(|(id, _)| *id == state.current_snapshot)
                .unwrap_or(0);
            state.message = if state.show_tree_page {
                "history tree page shown".into()
            } else {
                "history tree page hidden".into()
            };
            Ok(())
        }
        "help" | "h" | "?" => {
            state.help = Some(actions::Help::default());
            Ok(())
        }
        "goto" if parts.len() == 2 => {
            let id = parts[1].parse::<usize>().context("history node required")?;
            state.restore_snapshot(id)?;
            state.tree_cursor = tree_lines(state)
                .iter()
                .position(|(line_id, _)| *line_id == id)
                .unwrap_or(0);
            state.message = format!("history: moved to {id}");
            state.add_history(state.message.clone());
            Ok(())
        }
        _ => {
            state.message = format!("unknown command: {line}");
            Ok(())
        }
    }?;
    Ok(UiAction::Redraw)
}

fn rotate(state: &mut State, degrees: i32, use_selection: bool) -> Result<()> {
    let targets = state.targets(use_selection)?;
    let out = state
        .temp
        .path()
        .join(format!("rotate-{}.pdf", state.next_snapshot));
    let pdf = state.current_pdf()?.to_path_buf();
    let mut command = Command::new("qpdf");
    command.arg("--warning-exit-0").arg(&pdf);
    for page in &targets {
        let current = page_rotation(&pdf, page + 1)?;
        let next = (current + degrees).rem_euclid(360);
        command.arg(format!("--rotate={next}:{}", page + 1));
    }
    run(command.arg(&out))?;
    let direction = if degrees > 0 { "right" } else { "left" };
    state.replace_current(
        out,
        format!("rotate {direction}: page {}", page_list_label(targets)),
    )
}

fn delete_pages(state: &mut State, use_selection: bool) -> Result<()> {
    let targets = state.targets(use_selection)?;
    if targets.len() == state.page_count {
        state.current_pdf = None;
        state.page_count = 0;
        state.page = 0;
        state.selected.clear();
        state.clear_render_cache();
        return state.record_snapshot("delete all pages");
    }
    let keep: Vec<usize> = (0..state.page_count)
        .filter(|p| !targets.contains(p))
        .collect();
    let out = state
        .temp
        .path()
        .join(format!("delete-{}.pdf", state.next_snapshot));
    rebuild_from_ranges(state.current_pdf()?, &keep, &out)?;
    state.selected.clear();
    state.page = state.page.min(keep.len().saturating_sub(1));
    state.replace_current(out, format!("delete: page {}", page_list_label(targets)))
}

fn move_pages(state: &mut State, target_page_1: usize) -> Result<()> {
    let total = state.page_count;
    let targets = state.targets(true)?;
    if !(1..=total).contains(&target_page_1) {
        bail!("target page out of range");
    }
    let target = target_page_1.saturating_sub(1);
    if targets.contains(&target) {
        bail!("target is inside moved selection");
    }
    let target_set: BTreeSet<usize> = targets.iter().copied().collect();
    let remaining: Vec<usize> = (0..total).filter(|p| !target_set.contains(p)).collect();
    let insert_at = remaining
        .iter()
        .position(|&page| page == target)
        .ok_or_else(|| anyhow!("target page not found"))?;
    let mut order = Vec::new();
    order.extend_from_slice(&remaining[..insert_at]);
    order.extend_from_slice(&targets);
    order.extend_from_slice(&remaining[insert_at..]);
    let out = state
        .temp
        .path()
        .join(format!("move-{}.pdf", state.next_snapshot));
    rebuild_from_ranges(state.current_pdf()?, &order, &out)?;
    state.selected.clear();
    state.page = insert_at.min(order.len().saturating_sub(1));
    state.replace_current(
        out,
        format!(
            "move page {} before {target_page_1}",
            page_list_label(targets)
        ),
    )
}

fn move_pages_by(state: &mut State, delta: i32) -> Result<()> {
    let targets = state.targets(true)?;
    let target_set: BTreeSet<usize> = targets.iter().copied().collect();
    let old_page = state.page;
    let mut order = (0..state.page_count).collect::<Vec<_>>();
    if delta < 0 {
        let mut moved = false;
        for idx in 1..order.len() {
            if target_set.contains(&order[idx]) && !target_set.contains(&order[idx - 1]) {
                order.swap(idx - 1, idx);
                moved = true;
            }
        }
        if !moved {
            state.message = "already at beginning".into();
            return Ok(());
        }
    } else {
        let mut moved = false;
        for idx in (0..order.len().saturating_sub(1)).rev() {
            if target_set.contains(&order[idx]) && !target_set.contains(&order[idx + 1]) {
                order.swap(idx, idx + 1);
                moved = true;
            }
        }
        if !moved {
            state.message = "already at end".into();
            return Ok(());
        }
    }

    let out = state
        .temp
        .path()
        .join(format!("reorder-{}.pdf", state.next_snapshot));
    rebuild_from_ranges(state.current_pdf()?, &order, &out)?;
    state.selected = order
        .iter()
        .enumerate()
        .filter_map(|(idx, page)| target_set.contains(page).then_some(idx))
        .collect();
    state.page = order
        .iter()
        .position(|page| *page == old_page)
        .unwrap_or_else(|| state.selected.iter().next().copied().unwrap_or(0));
    let direction = if delta < 0 { "backward" } else { "forward" };
    state.replace_current(
        out,
        format!("move {direction}: page {}", page_list_label(targets)),
    )
}

fn mirror(state: &mut State, use_selection: bool) -> Result<()> {
    let targets = state.targets(use_selection)?;
    let mut replacements = BTreeMap::new();
    for page in &targets {
        let dest =
            state
                .temp
                .path()
                .join(format!("mirror-{}-{}.pdf", page + 1, state.next_snapshot));
        mirror_page_to_pdf(state.current_pdf()?, page + 1, &dest)?;
        replacements.insert(*page, dest);
    }
    let out = state
        .temp
        .path()
        .join(format!("mirror-{}.pdf", state.next_snapshot));
    rebuild_with_replacements(state.current_pdf()?, state.page_count, &replacements, &out)?;
    state.replace_current(out, format!("mirror: page {}", page_list_label(targets)))
}

fn crop_pages(state: &mut State, fmt: &str, use_selection: bool) -> Result<()> {
    let pages = state.targets(use_selection)?;
    let mut margins = [0_u32; 4];
    let mut edge = 0_usize;
    let names = ["left", "top", "right", "bottom"];
    let mut pending_input = None;
    loop {
        state.crop_preview = Some(CropPreview {
            margins,
            active_edge: edge,
        });
        state.message = format!(
            "crop {} LTRB={},{},{},{} | arrows/hjkl +5, HJKL +50, -5, _ -50, Tab edge",
            names[edge], margins[0], margins[1], margins[2], margins[3]
        );
        ui::show(state, fmt)?;
        let input = match pending_input.take() {
            Some(input) => input,
            None => actions::normal_input(read_input()?),
        };
        match input {
            Input::Esc | Input::Char('q') => {
                state.crop_preview = None;
                state.message = "crop cancelled".into();
                return Ok(());
            }
            Input::Enter => {
                state.crop_preview = None;
                break;
            }
            Input::Char('n') | Input::Char(' ') => {
                if pages.contains(&state.page) {
                    let next = pages
                        .iter()
                        .copied()
                        .find(|page| *page > state.page)
                        .unwrap_or_else(|| pages[0]);
                    state.page = next;
                } else {
                    state.page = (state.page + 1).min(state.page_count.saturating_sub(1));
                }
            }
            Input::Char('p') => {
                if pages.contains(&state.page) {
                    let prev = pages
                        .iter()
                        .copied()
                        .rev()
                        .find(|page| *page < state.page)
                        .unwrap_or_else(|| *pages.last().unwrap_or(&state.page));
                    state.page = prev;
                } else {
                    state.page = state.page.saturating_sub(1);
                }
            }
            Input::Left => {
                edge = 0;
                margins[edge] += 5;
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            Input::Right => {
                edge = 2;
                margins[edge] += 5;
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            Input::Up => {
                edge = 1;
                margins[edge] += 5;
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            Input::Down => {
                edge = 3;
                margins[edge] += 5;
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            Input::Tab | Input::Char('\t') => edge = (edge + 1) % 4,
            Input::Char('-') => {
                margins[edge] = margins[edge].saturating_sub(5);
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            Input::Char('_') => {
                margins[edge] = margins[edge].saturating_sub(50);
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            Input::Char(c) if matches!(c, 'h' | 'j' | 'k' | 'l' | 'H' | 'J' | 'K' | 'L') => {
                let amount = if c.is_uppercase() { 50 } else { 5 };
                edge = match c.to_ascii_lowercase() {
                    'h' => 0,
                    'k' => 1,
                    'l' => 2,
                    'j' => 3,
                    _ => edge,
                };
                margins[edge] += amount;
                pending_input = drain_pending_crop_adjust_inputs()?;
            }
            _ => {}
        }
    }
    let (max_width, max_height) = crop_preview_pixel_limits(state);
    let out = state
        .temp
        .path()
        .join(format!("crop-{}.pdf", state.next_snapshot));
    fs::copy(state.current_pdf()?, &out)
        .with_context(|| format!("copy crop source to {}", out.display()))?;
    for page in &pages {
        crop_page_boxes(&out, page + 1, margins, max_width, max_height)?;
    }
    state.replace_current(out, format!("crop: page {}", page_list_label(pages)))?;
    state.message = "crop applied without raster recompression; qpdf ok".into();
    Ok(())
}

fn drain_pending_crop_adjust_inputs() -> Result<Option<Input>> {
    while event::poll(Duration::from_millis(0))? {
        match actions::normal_input(read_input()?) {
            Input::Left
            | Input::Right
            | Input::Up
            | Input::Down
            | Input::Char('h' | 'j' | 'k' | 'l' | 'H' | 'J' | 'K' | 'L' | '-' | '_') => {}
            Input::Unknown => {}
            input => return Ok(Some(input)),
        }
    }
    Ok(None)
}

fn compress_pages(state: &mut State, level: &str, use_selection: bool) -> Result<()> {
    let targets = state.targets(use_selection)?;
    let (max_width, max_height, label) = compression_limits(level)?;
    let before_size = state.current_pdf().and_then(file_size).unwrap_or(0);
    let mut replacements = BTreeMap::new();
    for page in &targets {
        let dest =
            state
                .temp
                .path()
                .join(format!("compress-{}-{}.pdf", page + 1, state.next_snapshot));
        let page_size = pdf_page_size(state.current_pdf()?, page + 1)?;
        let bitmap = render_page_bitmap(state.current_pdf()?, page + 1, max_width, max_height)?;
        bitmap_to_pdf_with_page_size(&bitmap, &dest, page_size)?;
        replacements.insert(*page, dest);
    }
    let out = state
        .temp
        .path()
        .join(format!("compress-{}.pdf", state.next_snapshot));
    rebuild_with_replacements(state.current_pdf()?, state.page_count, &replacements, &out)?;
    state.replace_current(
        out,
        format!("compress {label}: page {}", page_list_label(targets)),
    )?;
    let after_size = state.current_pdf().and_then(file_size).unwrap_or(0);
    state.message = compression_message(label, before_size, after_size);
    Ok(())
}

fn compression_limits(level: &str) -> Result<(usize, usize, &'static str)> {
    match level {
        "strong" | "preview" | "screen" | "small" | "low" => Ok((900, 1200, "strong")),
        "normal" | "email" | "medium" | "ebook" | "default" => Ok((1400, 1900, "normal")),
        "light" | "archive" | "print" | "printer" | "high" => Ok((2000, 2800, "light")),
        _ => bail!("compression level must be light, normal, or strong"),
    }
}

fn compression_message(label: &str, before: u64, after: u64) -> String {
    if before == 0 || after == 0 {
        return format!("compressed with {label}; qpdf ok");
    }
    let before_label = human_size(before);
    let after_label = human_size(after);
    if after <= before {
        let saved = before - after;
        let percent = saved as f64 * 100.0 / before as f64;
        format!("compressed {label}: {before_label} -> {after_label} ({percent:.1}% smaller)")
    } else {
        let larger = after - before;
        let percent = larger as f64 * 100.0 / before as f64;
        format!("compressed {label}: {before_label} -> {after_label} ({percent:.1}% larger; undo if needed)")
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn file_size(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)?.len())
}

fn scan_pages(state: &mut State, dpi: Option<u32>) -> Result<()> {
    let mut options = state.config.scanning.clone();
    if let Some(dpi) = dpi {
        options.dpi = dpi;
    }
    let cached = state
        .scan_device
        .as_ref()
        .and_then(scanning::CachedDevice::device);
    state.scan = Some(scanning::Session::start_cached(
        options,
        state.temp.path(),
        cached,
    )?);
    state.show_tree_page = false;
    state.listing_page = None;
    state.help = None;
    Ok(())
}

fn handle_scan_input(state: &mut State, input: Input) -> Result<UiAction> {
    let mut session = state.scan.take().expect("scan session exists");
    let mut changed = false;
    if let Some(job) = &session.job {
        let (progress, outcome) = job.poll();
        changed = session.progress != progress;
        session.progress = progress;
        if let Some(outcome) = outcome {
            match outcome {
                scanning::Outcome::Done => {
                    let result = append_pdf_file(state, &job.output, false)
                        .and_then(|_| state.record_snapshot("scan page"));
                    match result {
                        Ok(()) => {
                            if let Some(device) = &session.progress.device {
                                state.scan_device =
                                    Some(scanning::CachedDevice::remember(device.clone()));
                            }
                            session.pages_added += 1;
                            session.ready = true;
                            state.message = format!(
                                "Scanned {} page{} · showing page {}",
                                session.pages_added,
                                plural(session.pages_added),
                                state.page + 1
                            );
                        }
                        Err(error) => session.error = Some(format!("{error:#}")),
                    }
                }
                scanning::Outcome::Cancelled => {
                    state.message = "Scan cancelled · completed pages kept".into();
                    return Ok(UiAction::Redraw);
                }
                scanning::Outcome::Failed(error) => {
                    state.scan_device = None;
                    session.error = Some(error);
                }
            }
            session.job = None;
            changed = true;
        }
    }
    match actions::normal_input(input) {
        Input::Esc | Input::CtrlC => {
            if let Some(job) = &session.job {
                job.cancel();
                session.cancelling = true;
                changed = true;
            } else {
                return Ok(UiAction::Redraw);
            }
        }
        Input::Enter | Input::Char('r' | 'S') if session.job.is_none() => {
            let cached = if session.error.is_none() {
                state
                    .scan_device
                    .as_ref()
                    .and_then(scanning::CachedDevice::device)
            } else {
                None
            };
            let pages_added = session.pages_added;
            session = scanning::Session::start_cached(session.options, state.temp.path(), cached)?;
            session.pages_added = pages_added;
            changed = true;
        }
        Input::Resize => {
            if let Some(ui) = &mut state.ui {
                ui.invalidate();
            }
            changed = true;
        }
        _ => {}
    }
    state.scan = Some(session);
    Ok(if changed {
        UiAction::Redraw
    } else {
        UiAction::Noop
    })
}

fn add_command(state: &mut State, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return list_addable_files(state, None);
    }
    if paths.len() == 1 {
        let path = expand_path(PathBuf::from(&paths[0]))?;
        if path.is_dir() {
            return list_addable_files(state, Some(&path.display().to_string()));
        }
    }
    add_files(state, paths)
}

fn add_files(state: &mut State, paths: &[String]) -> Result<()> {
    let mut added = 0;
    for raw in paths {
        let path = expand_path(PathBuf::from(raw))?;
        ensure_file(&path)?;
        if is_image(&path) {
            let pdf = state.temp.path().join(format!("image-add-{added}.pdf"));
            image_to_pdf(&path, &pdf)?;
            append_pdf_file(state, &pdf, false)?;
            added += 1;
        } else if is_pdf(&path) {
            let before = state.page_count;
            append_pdf_file(state, &path, false)?;
            added += state.page_count - before;
        } else {
            bail!("unsupported add file type: {}", path.display());
        }
    }
    if added > 0 {
        state.record_snapshot(format!("add: {added} page{}", plural(added)))?;
        state.message = format!("added {added} page{}", plural(added));
    }
    Ok(())
}

fn list_addable_files(state: &mut State, raw_dir: Option<&str>) -> Result<()> {
    let dir = expand_path(PathBuf::from(raw_dir.unwrap_or(".")))?;
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    let mut dirs: Vec<ListingEntry> = Vec::new();
    let mut files: Vec<ListingEntry> = Vec::new();
    if let Some(parent) = dir.parent() {
        dirs.push(ListingEntry {
            label: "../".into(),
            path: parent.to_path_buf(),
            kind: ListingEntryKind::Directory,
        });
    }
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            dirs.push(ListingEntry {
                label: format!("{name}/"),
                path,
                kind: ListingEntryKind::Directory,
            });
        } else if is_image(&path) || is_pdf(&path) {
            files.push(ListingEntry {
                label: name.to_string(),
                path,
                kind: ListingEntryKind::File,
            });
        }
    }
    dirs.sort_by_key(|item| item.label.to_ascii_lowercase());
    files.sort_by_key(|item| item.label.to_ascii_lowercase());
    let mut entries = Vec::new();
    entries.extend(dirs);
    entries.extend(files);
    if entries.is_empty() {
        entries.push(ListingEntry {
            label: "(no addable PDFs/images here)".into(),
            path: dir.clone(),
            kind: ListingEntryKind::Empty,
        });
    }
    state.show_tree_page = false;
    state.help = None;
    state.listing_page = Some(ListingPage {
        title: format!("Addable files in {} | Enter add/open", dir.display()),
        entries,
        selected: 0,
        marked: BTreeSet::new(),
    });
    state.message = "file list shown; Esc closes".into();
    Ok(())
}

fn activate_listing_selection(state: &mut State) -> Result<()> {
    let Some(listing) = &state.listing_page else {
        return Ok(());
    };
    if !listing.marked.is_empty() {
        let paths = listing
            .marked
            .iter()
            .filter_map(|idx| listing.entries.get(*idx))
            .filter(|entry| entry.kind == ListingEntryKind::File)
            .map(|entry| entry.path.display().to_string())
            .collect::<Vec<_>>();
        state.listing_page = None;
        return add_files(state, &paths);
    }
    let Some(entry) = listing.entries.get(listing.selected) else {
        return Ok(());
    };
    match entry.kind {
        ListingEntryKind::Directory => {
            let path = entry.path.clone();
            list_addable_files(state, Some(&path.display().to_string()))
        }
        ListingEntryKind::File => {
            let path = entry.path.clone();
            state.listing_page = None;
            add_files(state, &[path.display().to_string()])
        }
        ListingEntryKind::Empty => Ok(()),
    }
}

fn append_pdf_file(state: &mut State, path: &Path, record: bool) -> Result<()> {
    qpdf_check(path)?;
    let out = state.temp.path().join(format!(
        "append-{}-{}.pdf",
        state.next_snapshot, state.page_count
    ));
    if let Some(current) = &state.current_pdf {
        run(Command::new("qpdf")
            .arg("--warning-exit-0")
            .arg("--empty")
            .arg("--pages")
            .arg(current)
            .arg("1-z")
            .arg(path)
            .arg("1-z")
            .arg("--")
            .arg(&out))?;
    } else {
        fs::copy(path, &out)?;
    }
    qpdf_check(&out)?;
    let before = state.page_count;
    state.current_pdf = Some(out);
    state.page_count = page_count(state.current_pdf()?)?;
    state.page = before.min(state.page_count.saturating_sub(1));
    state.clear_render_cache();
    state.clamp_page();
    if record {
        state.record_snapshot(format!("add PDF: {}", path.display()))?;
    }
    Ok(())
}

fn undo(state: &mut State) -> Result<()> {
    let parent = state
        .snapshots
        .get(&state.current_snapshot)
        .and_then(|s| s.parent);
    let Some(parent) = parent else {
        state.message = "nothing to undo".into();
        return Ok(());
    };
    state.restore_snapshot(parent)?;
    state.message = format!("undo: {}", state.snapshots[&state.current_snapshot].label);
    state.add_history(state.message.clone());
    Ok(())
}

fn redo(state: &mut State) -> Result<()> {
    let child = state
        .snapshots
        .get(&state.current_snapshot)
        .and_then(|s| s.children.last().copied());
    let Some(child) = child else {
        state.message = "nothing to redo".into();
        return Ok(());
    };
    state.restore_snapshot(child)?;
    state.message = format!("redo: {}", state.snapshots[&child].label);
    state.add_history(state.message.clone());
    Ok(())
}

fn save_command(state: &mut State, args: &[String]) -> Result<()> {
    let out = match args {
        [] => None,
        _ => Some(resolve_output_target(&state.out, args)?),
    };
    save(state, out)
}

fn save_as_command(state: &mut State, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: :save-as PATH");
    }
    let out = resolve_output_target(&state.out, args)?;
    save(state, Some(out))
}

fn rename_command(state: &mut State, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: :rename NAME [DIR]");
    }
    let out = resolve_output_target(&state.out, args)?;
    rename_output(state, out)
}

fn save(state: &mut State, out: Option<PathBuf>) -> Result<()> {
    let out = if state.overwrite {
        if out.is_some() {
            state.overwrite = false;
        }
        out.unwrap_or_else(|| {
            state
                .original
                .as_ref()
                .expect("overwrite has original path")
                .clone()
        })
    } else if let Some(out) = out {
        out
    } else {
        state.out.clone()
    };
    if save_to_path(state, out.clone())? {
        state.out = out;
        state.overwrite = false;
    }
    Ok(())
}

fn rename_output(state: &mut State, out: PathBuf) -> Result<()> {
    let out = pdf_output_path(expand_path(out)?)?;
    if out == state.out {
        state.message = format!("output already named {}", out.display());
        return Ok(());
    }
    if state.out.exists() {
        if out.exists()
            && !confirm(
                state,
                &format!(
                    "Rename to {}? File exists; overwrite? [y/N] ",
                    out.display()
                ),
            )?
        {
            state.message = format!("rename cancelled: {}", out.display());
            return Ok(());
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&state.out, &out)
            .with_context(|| format!("rename {} to {}", state.out.display(), out.display()))?;
        state.add_history(format!("rename: {}", out.display()));
        state.message = format!("renamed to {}", out.display());
    } else {
        state.add_history(format!("rename target: {}", out.display()));
        state.message = format!("output target renamed to {}", out.display());
    }
    state.out = out;
    state.overwrite = false;
    Ok(())
}

fn resolve_output_target(current_out: &Path, args: &[String]) -> Result<PathBuf> {
    match args {
        [] => bail!("usage: :save [PATH] or :save NAME DIR"),
        [one] => {
            let raw = PathBuf::from(one);
            let expanded = expand_path(raw)?;
            if expanded.exists() && expanded.is_dir() {
                let file_name = current_out
                    .file_name()
                    .ok_or_else(|| anyhow!("current output path must include a file name"))?;
                return pdf_output_path(expanded.join(file_name));
            }
            let target = if Path::new(one).components().count() == 1 {
                current_out
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(one)
            } else {
                expanded
            };
            pdf_output_path(target)
        }
        [name, dir] => {
            let name_path = Path::new(name);
            if name_path.components().count() != 1 {
                bail!("NAME must be a file name when DIR is provided");
            }
            pdf_output_path(expand_path(PathBuf::from(dir))?.join(name))
        }
        _ => bail!("usage: :save [PATH] or :save NAME DIR"),
    }
}

fn pdf_output_path(path: PathBuf) -> Result<PathBuf> {
    if path.extension().is_none() {
        let mut name = path
            .file_name()
            .ok_or_else(|| anyhow!("output path must include a file name"))?
            .to_os_string();
        name.push(".pdf");
        Ok(path.with_file_name(name))
    } else {
        Ok(path)
    }
}

fn save_to_path(state: &mut State, out: PathBuf) -> Result<bool> {
    let current = state.current_pdf()?.to_path_buf();
    if out.exists()
        && Some(&out) != state.original.as_ref()
        && !confirm(
            state,
            &format!("Save to {}? File exists; overwrite? [y/N] ", out.display()),
        )?
    {
        state.message = format!("save cancelled: {}", out.display());
        return Ok(false);
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = Builder::new()
        .prefix(&format!(
            ".{}.",
            out.file_name().and_then(OsStr::to_str).unwrap_or("pdfview")
        ))
        .suffix(".pdf")
        .tempfile_in(out.parent().unwrap_or_else(|| Path::new(".")))?;
    write_saved_pdf(&current, tmp.path())?;
    qpdf_check(tmp.path())?;
    tmp.persist(&out).map_err(|e| anyhow!(e.error))?;
    state.saved_snapshot = state.current_snapshot;
    state.dirty = false;
    state.message = format!("saved to {}; qpdf ok", out.display());
    state.add_history(format!("save: {}", out.display()));
    Ok(true)
}

fn write_saved_pdf(input: &Path, output: &Path) -> Result<()> {
    run(Command::new("qpdf")
        .arg("--warning-exit-0")
        .arg(format!("--min-version={SAVED_PDF_MIN_VERSION}"))
        .arg(input)
        .arg(output))
}

struct Bitmap {
    width: usize,
    height: usize,
    stride: usize,
    data: Vec<u8>,
}

fn prefetch_pages(state: &State, cols: u16, rows: u16, fmt: &str) {
    let Ok(pdf) = state.current_pdf().map(Path::to_path_buf) else {
        return;
    };
    let cache = Arc::clone(&state.render_cache);
    let pending = Arc::clone(&state.pending_renders);
    let snapshot = state.current_snapshot;
    let current = state.page;
    let total = state.page_count;
    let fmt = fmt.to_string();
    thread::spawn(move || {
        let mut pages = Vec::new();
        for offset in [1_isize, -1, 2, -2] {
            let page = current as isize + offset;
            if page >= 0 && (page as usize) < total {
                pages.push(page as usize);
            }
        }
        for page in pages {
            let key = RenderKey {
                snapshot,
                page,
                cols,
                rows,
                format: fmt.clone(),
            };
            render_key_into_cache(
                &cache,
                &pending,
                RenderRequest {
                    key,
                    pdf: pdf.clone(),
                    page: page + 1,
                    cols,
                    rows,
                    format: fmt.clone(),
                },
            );
        }
    });
}

fn spawn_render_if_needed(
    cache: Arc<Mutex<HashMap<RenderKey, Vec<u8>>>>,
    pending: Arc<Mutex<HashSet<RenderKey>>>,
    request: RenderRequest,
) {
    thread::spawn(move || {
        render_key_into_cache(&cache, &pending, request);
    });
}

fn render_key_into_cache(
    cache: &Arc<Mutex<HashMap<RenderKey, Vec<u8>>>>,
    pending: &Arc<Mutex<HashSet<RenderKey>>>,
    request: RenderRequest,
) {
    let key = request.key.clone();
    if cache.lock().is_ok_and(|cache| cache.contains_key(&key)) {
        return;
    }
    {
        let Ok(mut pending) = pending.lock() else {
            return;
        };
        if !pending.insert(key.clone()) {
            return;
        }
    }
    if let Ok(bytes) = render_page_output(
        &request.pdf,
        request.page,
        request.cols,
        request.rows,
        &request.format,
    ) {
        if let Ok(mut cache) = cache.lock() {
            cache.entry(key.clone()).or_insert(bytes);
        }
    }
    if let Ok(mut pending) = pending.lock() {
        pending.remove(&key);
    }
}

fn render_page_output(pdf: &Path, page: usize, cols: u16, rows: u16, fmt: &str) -> Result<Vec<u8>> {
    if fmt != "kitty" {
        bail!("native renderer currently supports kitty/ghostty image protocol only");
    }
    let (pixel_width, pixel_height) = render_pixel_limits(cols, rows);
    let bitmap = render_page_bitmap(pdf, page, pixel_width, pixel_height)?;
    Ok(kitty_image_bytes(&bitmap, cols, rows))
}

fn render_page_output_with_crop_preview(
    pdf: &Path,
    page: usize,
    cols: u16,
    rows: u16,
    fmt: &str,
    preview: CropPreview,
) -> Result<Vec<u8>> {
    if fmt != "kitty" {
        bail!("native renderer currently supports kitty/ghostty image protocol only");
    }
    let (pixel_width, pixel_height) = render_pixel_limits(cols, rows);
    let mut bitmap = render_page_bitmap(pdf, page, pixel_width, pixel_height)?;
    draw_crop_overlay(&mut bitmap, preview);
    Ok(kitty_image_bytes(&bitmap, cols, rows))
}

fn render_pixel_limits(cols: u16, rows: u16) -> (usize, usize) {
    let (cell_width, cell_height) = cell_pixel_size();
    (
        (cols as usize).saturating_mul(cell_width).clamp(1, 2400),
        (rows as usize).saturating_mul(cell_height).clamp(1, 3200),
    )
}

fn cell_pixel_size() -> (usize, usize) {
    terminal::window_size()
        .ok()
        .filter(|size| size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0)
        .map(|size| {
            (
                (size.width / size.columns).max(1) as usize,
                (size.height / size.rows).max(1) as usize,
            )
        })
        .unwrap_or((10, 20))
}

fn crop_preview_pixel_limits(_state: &State) -> (usize, usize) {
    let (cols, rows) = terminal::size().unwrap_or((100, 40));
    let area = ui::document_area(ratatui::layout::Rect::new(0, 0, cols, rows));
    render_pixel_limits(area.width, area.height)
}

fn crop_page_boxes(
    pdf: &Path,
    page: usize,
    margins: [u32; 4],
    max_width: usize,
    max_height: usize,
) -> Result<()> {
    let page_size = pdf_page_size(pdf, page)?;
    let scale = render_scale(page_size, max_width, max_height);
    let margins = [
        margins[0] as f64 / scale,
        margins[1] as f64 / scale,
        margins[2] as f64 / scale,
        margins[3] as f64 / scale,
    ];
    let page_box = page_box(pdf, page)?;
    let min_size = 12.0;
    let left = (page_box.left + margins[0]).min(page_box.right - min_size);
    let top = (page_box.top - margins[1]).max(page_box.bottom + min_size);
    let right = (page_box.right - margins[2]).max(left + min_size);
    let bottom = (page_box.bottom + margins[3]).min(top - min_size);
    if right - left < min_size || top - bottom < min_size {
        bail!("crop leaves too little page content");
    }
    update_page_box(
        pdf,
        page,
        PageBox {
            left,
            bottom,
            right,
            top,
        },
    )
}

fn render_scale(page_size: PageSize, max_width: usize, max_height: usize) -> f64 {
    (max_width as f64 / page_size.width)
        .min(max_height as f64 / page_size.height)
        .max(0.1)
}

fn pdf_page_size(pdf: &Path, page: usize) -> Result<PageSize> {
    let uri = CString::new(path_to_file_uri(pdf))?;
    let document = unsafe {
        poppler_document_new_from_file(uri.as_ptr(), std::ptr::null(), std::ptr::null_mut())
    };
    if document.is_null() {
        bail!("Poppler could not open {}", pdf.display());
    }
    let page_ptr = unsafe { poppler_document_get_page(document, (page - 1) as c_int) };
    if page_ptr.is_null() {
        unsafe { g_object_unref(document) };
        bail!("PDF page not found: {page}");
    }
    let mut width = 0.0;
    let mut height = 0.0;
    unsafe {
        poppler_page_get_size(page_ptr, &mut width, &mut height);
        g_object_unref(page_ptr);
        g_object_unref(document);
    }
    Ok(PageSize { width, height })
}

fn page_box(pdf: &Path, page: usize) -> Result<PageBox> {
    let object = page_object(pdf, page)?;
    let json = qpdf_json_for_object(pdf, &object)?;
    let object_key = format!("obj:{object}");
    let value = json
        .get("qpdf")
        .and_then(Value::as_array)
        .and_then(|items| items.get(1))
        .and_then(|objects| objects.get(&object_key))
        .and_then(|object| object.get("value"))
        .ok_or_else(|| anyhow!("qpdf JSON did not include page object {object}"))?;
    let media = value
        .get("/CropBox")
        .or_else(|| value.get("/MediaBox"))
        .ok_or_else(|| anyhow!("page {page} has no MediaBox"))?;
    parse_page_box(media)
}

fn page_object(pdf: &Path, page: usize) -> Result<String> {
    let output = Command::new("qpdf")
        .arg("--json")
        .arg("--json-key=pages")
        .arg(pdf)
        .output()
        .with_context(|| "run qpdf --json")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let json: Value = serde_json::from_slice(&output.stdout).context("parse qpdf pages JSON")?;
    json.get("pages")
        .and_then(Value::as_array)
        .and_then(|pages| pages.get(page - 1))
        .and_then(|page| page.get("object"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("qpdf JSON did not include page {page}"))
}

fn qpdf_json_for_object(pdf: &Path, object: &str) -> Result<Value> {
    let output = Command::new("qpdf")
        .arg("--json")
        .arg(format!("--json-object={object}"))
        .arg(pdf)
        .output()
        .with_context(|| "run qpdf --json-object")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    serde_json::from_slice(&output.stdout).context("parse qpdf object JSON")
}

fn parse_page_box(value: &Value) -> Result<PageBox> {
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("page box is not an array"))?;
    if values.len() != 4 {
        bail!("page box should have 4 numbers");
    }
    let mut nums = [0.0; 4];
    for (idx, value) in values.iter().enumerate() {
        nums[idx] = value
            .as_f64()
            .ok_or_else(|| anyhow!("page box contains a non-number"))?;
    }
    Ok(PageBox {
        left: nums[0],
        bottom: nums[1],
        right: nums[2],
        top: nums[3],
    })
}

fn update_page_box(pdf: &Path, page: usize, page_box: PageBox) -> Result<()> {
    let object = page_object(pdf, page)?;
    let mut json = qpdf_json_for_object(pdf, &object)?;
    let object_key = format!("obj:{object}");
    let objects = json
        .get_mut("qpdf")
        .and_then(Value::as_array_mut)
        .and_then(|items| items.get_mut(1))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("qpdf JSON did not include object table"))?;
    let value = objects
        .get_mut(&object_key)
        .and_then(|object| object.get_mut("value"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("qpdf JSON did not include page object {object}"))?;
    let box_value =
        serde_json::json!([page_box.left, page_box.bottom, page_box.right, page_box.top]);
    value.insert("/MediaBox".into(), box_value.clone());
    value.insert("/CropBox".into(), box_value);

    let update = pdf.with_file_name(format!(
        ".{}.page-{page}.json",
        pdf.file_name().and_then(OsStr::to_str).unwrap_or("pdfview")
    ));
    fs::write(&update, serde_json::to_vec(&json)?)
        .with_context(|| format!("write {}", update.display()))?;
    let result = run(Command::new("qpdf")
        .arg(pdf)
        .arg(format!("--update-from-json={}", update.display()))
        .arg("--replace-input"));
    let _ = fs::remove_file(&update);
    result?;
    qpdf_check(pdf)
}

fn draw_crop_overlay(bitmap: &mut Bitmap, preview: CropPreview) {
    let left = preview.margins[0].min(bitmap.width as u32) as usize;
    let top = preview.margins[1].min(bitmap.height as u32) as usize;
    let right = preview.margins[2].min(bitmap.width as u32) as usize;
    let bottom = preview.margins[3].min(bitmap.height as u32) as usize;
    shade_rect(bitmap, 0, 0, left, bitmap.height);
    shade_rect(bitmap, 0, 0, bitmap.width, top);
    shade_rect(
        bitmap,
        bitmap.width.saturating_sub(right),
        0,
        right,
        bitmap.height,
    );
    shade_rect(
        bitmap,
        0,
        bitmap.height.saturating_sub(bottom),
        bitmap.width,
        bottom,
    );
    draw_vline(bitmap, left, [220, 0, 0, 255]);
    draw_hline(bitmap, top, [220, 0, 0, 255]);
    draw_vline(bitmap, bitmap.width.saturating_sub(right), [220, 0, 0, 255]);
    draw_hline(
        bitmap,
        bitmap.height.saturating_sub(bottom),
        [220, 0, 0, 255],
    );
}

fn shade_rect(bitmap: &mut Bitmap, x: usize, y: usize, width: usize, height: usize) {
    let max_y = (y + height).min(bitmap.height);
    let max_x = (x + width).min(bitmap.width);
    for yy in y..max_y {
        for xx in x..max_x {
            blend_pixel(bitmap, xx, yy, [180, 0, 0, 90]);
        }
    }
}

fn draw_vline(bitmap: &mut Bitmap, x: usize, color: [u8; 4]) {
    if x >= bitmap.width {
        return;
    }
    for dx in x.saturating_sub(1)..=(x + 1).min(bitmap.width - 1) {
        for y in 0..bitmap.height {
            set_pixel(bitmap, dx, y, color);
        }
    }
}

fn draw_hline(bitmap: &mut Bitmap, y: usize, color: [u8; 4]) {
    if y >= bitmap.height {
        return;
    }
    for dy in y.saturating_sub(1)..=(y + 1).min(bitmap.height - 1) {
        for x in 0..bitmap.width {
            set_pixel(bitmap, x, dy, color);
        }
    }
}

fn blend_pixel(bitmap: &mut Bitmap, x: usize, y: usize, rgba: [u8; 4]) {
    let idx = y * bitmap.stride + x * 4;
    let alpha = rgba[3] as u16;
    bitmap.data[idx] =
        ((bitmap.data[idx] as u16 * (255 - alpha) + rgba[2] as u16 * alpha) / 255) as u8;
    bitmap.data[idx + 1] =
        ((bitmap.data[idx + 1] as u16 * (255 - alpha) + rgba[1] as u16 * alpha) / 255) as u8;
    bitmap.data[idx + 2] =
        ((bitmap.data[idx + 2] as u16 * (255 - alpha) + rgba[0] as u16 * alpha) / 255) as u8;
    bitmap.data[idx + 3] = 255;
}

fn set_pixel(bitmap: &mut Bitmap, x: usize, y: usize, rgba: [u8; 4]) {
    let idx = y * bitmap.stride + x * 4;
    bitmap.data[idx] = rgba[2];
    bitmap.data[idx + 1] = rgba[1];
    bitmap.data[idx + 2] = rgba[0];
    bitmap.data[idx + 3] = rgba[3];
}

fn mirror_page_to_pdf(pdf: &Path, page: usize, out: &Path) -> Result<()> {
    let page_size = pdf_page_size(pdf, page)?;
    let mut bitmap = render_page_bitmap_at_dpi(pdf, page, RASTER_EDIT_DPI)?;
    flip_bitmap_horizontally(&mut bitmap);
    bitmap_to_pdf_with_page_size(&bitmap, out, page_size)
}

fn flip_bitmap_horizontally(bitmap: &mut Bitmap) {
    for y in 0..bitmap.height {
        let row = &mut bitmap.data[y * bitmap.stride..][..bitmap.width * 4];
        for x in 0..bitmap.width / 2 {
            let left = x * 4;
            let right = (bitmap.width - 1 - x) * 4;
            for offset in 0..4 {
                row.swap(left + offset, right + offset);
            }
        }
    }
}

#[cfg(test)]
fn bitmap_to_pdf(bitmap: &Bitmap, out: &Path) -> Result<()> {
    bitmap_to_pdf_with_page_size(
        bitmap,
        out,
        PageSize {
            width: bitmap.width as f64,
            height: bitmap.height as f64,
        },
    )
}

fn bitmap_to_pdf_with_page_size(bitmap: &Bitmap, out: &Path, page_size: PageSize) -> Result<()> {
    let path = CString::new(out.to_string_lossy().as_bytes())?;
    let pdf_surface = unsafe {
        cairo_pdf_surface_create(
            path.as_ptr(),
            page_size.width as c_double,
            page_size.height as c_double,
        )
    };
    if pdf_surface.is_null() {
        bail!("create cairo PDF surface failed");
    }
    let image_surface = unsafe {
        cairo_image_surface_create_for_data(
            bitmap.data.as_ptr() as *mut u8,
            CAIRO_FORMAT_ARGB32,
            bitmap.width as c_int,
            bitmap.height as c_int,
            bitmap.stride as c_int,
        )
    };
    if image_surface.is_null() {
        unsafe { cairo_surface_destroy(pdf_surface) };
        bail!("create cairo image surface failed");
    }
    let cr = unsafe { cairo_create(pdf_surface) };
    unsafe {
        cairo_scale(
            cr,
            page_size.width / bitmap.width as f64,
            page_size.height / bitmap.height as f64,
        );
        cairo_set_source_surface(cr, image_surface, 0.0, 0.0);
        cairo_paint(cr);
        cairo_destroy(cr);
        cairo_surface_destroy(image_surface);
        cairo_surface_destroy(pdf_surface);
    }
    qpdf_check(out)
}

fn render_page_bitmap_at_dpi(pdf: &Path, page: usize, dpi: f64) -> Result<Bitmap> {
    let page_size = pdf_page_size(pdf, page)?;
    let max_width = (page_size.width * dpi / 72.0).ceil().max(1.0) as usize;
    let max_height = (page_size.height * dpi / 72.0).ceil().max(1.0) as usize;
    render_page_bitmap(pdf, page, max_width, max_height)
}

fn render_page_bitmap(
    pdf: &Path,
    page: usize,
    max_width: usize,
    max_height: usize,
) -> Result<Bitmap> {
    let uri = CString::new(path_to_file_uri(pdf))?;
    let document = unsafe {
        poppler_document_new_from_file(uri.as_ptr(), std::ptr::null(), std::ptr::null_mut())
    };
    if document.is_null() {
        bail!("Poppler could not open {}", pdf.display());
    }
    let page_ptr = unsafe { poppler_document_get_page(document, (page - 1) as c_int) };
    if page_ptr.is_null() {
        unsafe { g_object_unref(document) };
        bail!("PDF page not found: {page}");
    }
    let mut page_width = 0.0;
    let mut page_height = 0.0;
    unsafe { poppler_page_get_size(page_ptr, &mut page_width, &mut page_height) };
    let scale = (max_width as f64 / page_width)
        .min(max_height as f64 / page_height)
        .max(0.1);
    let width = (page_width * scale).ceil().max(1.0) as usize;
    let height = (page_height * scale).ceil().max(1.0) as usize;
    let surface =
        unsafe { cairo_image_surface_create(CAIRO_FORMAT_ARGB32, width as c_int, height as c_int) };
    if surface.is_null() {
        unsafe {
            g_object_unref(page_ptr);
            g_object_unref(document);
        }
        bail!("create cairo surface failed");
    }
    let cr = unsafe { cairo_create(surface) };
    unsafe {
        cairo_set_source_rgb(cr, 1.0, 1.0, 1.0);
        cairo_paint(cr);
        cairo_scale(cr, scale, scale);
        poppler_page_render(page_ptr, cr);
        cairo_surface_flush(surface);
    }
    let stride = unsafe { cairo_image_surface_get_stride(surface) as usize };
    let src = unsafe { cairo_image_surface_get_data(surface) };
    if src.is_null() {
        unsafe {
            cairo_destroy(cr);
            cairo_surface_destroy(surface);
            g_object_unref(page_ptr);
            g_object_unref(document);
        }
        bail!("read cairo surface failed");
    }
    let data = unsafe { std::slice::from_raw_parts(src, stride * height) }.to_vec();
    unsafe {
        cairo_destroy(cr);
        cairo_surface_destroy(surface);
        g_object_unref(page_ptr);
        g_object_unref(document);
    }
    Ok(Bitmap {
        width,
        height,
        stride,
        data,
    })
}

fn kitty_image_bytes(bitmap: &Bitmap, cols: u16, rows: u16) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bitmap.width * bitmap.height * 4);
    for y in 0..bitmap.height {
        let row = &bitmap.data[y * bitmap.stride..][..bitmap.width * 4];
        for px in row.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    let encoded = base64_encode(&rgba);
    let mut out = Vec::with_capacity(encoded.len() + 128);
    let (cell_width, cell_height) = cell_pixel_size();
    let display_cols = bitmap
        .width
        .div_ceil(cell_width)
        .clamp(1, cols.max(1) as usize);
    let display_rows = bitmap
        .height
        .div_ceil(cell_height)
        .clamp(1, rows.max(1) as usize);
    let header = format!(
        "\x1b_Ga=T,f=32,s={},v={},c={},r={},q=2,m=0;",
        bitmap.width, bitmap.height, display_cols, display_rows
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(encoded.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn path_to_file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                uri.push(*byte as char)
            }
            b => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

fn rebuild_from_ranges(pdf: &Path, zero_based_pages: &[usize], out: &Path) -> Result<()> {
    let mut cmd = Command::new("qpdf");
    cmd.arg("--empty").arg("--pages");
    for page in zero_based_pages {
        cmd.arg(pdf).arg((page + 1).to_string());
    }
    cmd.arg("--").arg(out);
    run(&mut cmd)?;
    qpdf_check(out)
}

fn rebuild_with_replacements(
    pdf: &Path,
    page_count: usize,
    replacements: &BTreeMap<usize, PathBuf>,
    out: &Path,
) -> Result<()> {
    let mut cmd = Command::new("qpdf");
    cmd.arg("--empty").arg("--pages");
    let mut page = 0;
    while page < page_count {
        if let Some(replacement) = replacements.get(&page) {
            qpdf_check(replacement)?;
            cmd.arg(replacement).arg("1");
            page += 1;
            continue;
        }
        let start = page;
        while page < page_count && !replacements.contains_key(&page) {
            page += 1;
        }
        cmd.arg(pdf).arg(qpdf_range(start, page - 1));
    }
    cmd.arg("--").arg(out);
    run(&mut cmd)?;
    qpdf_check(out)
}

fn qpdf_range(start: usize, end: usize) -> String {
    if start == end {
        (start + 1).to_string()
    } else {
        format!("{}-{}", start + 1, end + 1)
    }
}

#[cfg(test)]
fn concat_pdfs(parts: &[PathBuf], out: &Path) -> Result<()> {
    let mut cmd = Command::new("qpdf");
    cmd.arg("--empty").arg("--pages");
    for part in parts {
        cmd.arg(part).arg("1-z");
    }
    cmd.arg("--").arg(out);
    run(&mut cmd)?;
    qpdf_check(out)
}

fn image_to_pdf(image: &Path, out: &Path) -> Result<()> {
    require_tool("magick")?;
    run(Command::new("magick")
        .arg(image)
        .arg("-auto-orient")
        .arg("-density")
        .arg(IMAGE_IMPORT_DPI)
        .arg(out))
}

fn qpdf_check(path: &Path) -> Result<()> {
    run(Command::new("qpdf")
        .arg("--warning-exit-0")
        .arg("--check")
        .arg(path)
        .stdout(Stdio::null()))
}

fn page_count(path: &Path) -> Result<usize> {
    let output = Command::new("pdfinfo")
        .arg(path)
        .output()
        .with_context(|| "run pdfinfo")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(raw) = line.strip_prefix("Pages:") {
            return raw.trim().parse().context("parse page count");
        }
    }
    bail!("pdfinfo did not report page count");
}

fn page_rotation(path: &Path, page: usize) -> Result<i32> {
    let output = Command::new("pdfinfo")
        .arg("-f")
        .arg(page.to_string())
        .arg("-l")
        .arg(page.to_string())
        .arg(path)
        .output()
        .with_context(|| "run pdfinfo")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("Page {page:>4} rot:");
    for line in stdout.lines() {
        if line.starts_with(&prefix) {
            let raw = line
                .split_once(':')
                .map(|(_, raw)| raw.trim())
                .unwrap_or_default();
            return raw.parse().context("parse page rotation");
        }
    }
    Ok(0)
}

fn run(cmd: &mut Command) -> Result<()> {
    let display = format!("{cmd:?}");
    let output = cmd.output().with_context(|| format!("run {display}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            stdout.trim()
        } else {
            detail
        };
        bail!(
            "{} failed with status {}: {}",
            display,
            output.status,
            detail.lines().next().unwrap_or("no output")
        );
    }
    Ok(())
}

fn require_tool(name: &str) -> Result<()> {
    let status = Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("look for {name}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{name} not found")
    }
}

fn ensure_file(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("not found: {}", path.display());
    }
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    Ok(())
}

fn expand_path(path: PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let expanded = if raw == "~" {
        dirs_home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs_home()?.join(rest)
    } else {
        path
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    })
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn native_format() -> String {
    if let Ok(format) = std::env::var("PDFVIEW_FORMAT") {
        return format;
    }
    "kitty".into()
}

fn parse_pages(spec: &str, total: usize) -> Result<BTreeSet<usize>> {
    let mut pages = BTreeSet::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            let mut start = a.parse::<usize>().context("invalid page range")?;
            let mut end = b.parse::<usize>().context("invalid page range")?;
            if start > end {
                std::mem::swap(&mut start, &mut end);
            }
            for page in start..=end {
                insert_page(&mut pages, page, total)?;
            }
        } else {
            insert_page(
                &mut pages,
                part.parse::<usize>().context("invalid page")?,
                total,
            )?;
        }
    }
    Ok(pages)
}

fn insert_page(pages: &mut BTreeSet<usize>, page: usize, total: usize) -> Result<()> {
    if page == 0 || page > total {
        bail!("page out of range: {page}");
    }
    pages.insert(page - 1);
    Ok(())
}

#[cfg(test)]
fn page_range(pages: &[usize]) -> String {
    pages
        .iter()
        .map(|p| (p + 1).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn page_list_label<I>(pages: I) -> String
where
    I: IntoIterator<Item = usize>,
{
    pages
        .into_iter()
        .map(|p| (p + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn compact_page_list_label<I>(pages: I) -> String
where
    I: IntoIterator<Item = usize>,
{
    let pages = pages.into_iter().collect::<Vec<_>>();
    if pages.len() > 8 {
        format!("{} pages [{}]", pages.len(), page_ranges_label(pages))
    } else {
        page_list_label(pages)
    }
}

fn page_ranges_label<I>(pages: I) -> String
where
    I: IntoIterator<Item = usize>,
{
    let pages = pages.into_iter().collect::<Vec<_>>();
    if pages.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    let mut start = pages[0];
    let mut prev = pages[0];
    for page in pages.into_iter().skip(1) {
        if page == prev + 1 {
            prev = page;
            continue;
        }
        parts.push(page_range_label(start, prev));
        start = page;
        prev = page;
    }
    parts.push(page_range_label(start, prev));
    parts.join(", ")
}

fn page_range_label(start: usize, end: usize) -> String {
    if start == end {
        (start + 1).to_string()
    } else {
        format!("{}-{}", start + 1, end + 1)
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn empty_state() -> Result<State> {
        State::new(
            Args {
                pdf: None,
                page: 1,
                out: None,
                overwrite: false,
                format: None,
                scan: false,
                scan_dpi: None,
            },
            tempfile::tempdir()?,
        )
    }

    #[test]
    fn successful_scan_is_undoable_and_later_failure_keeps_it() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut state = empty_state()?;
        let fixture = state.temp.path().join("fixture.pdf");
        bitmap_to_pdf_with_page_size(
            &Bitmap {
                width: 1,
                height: 1,
                stride: 4,
                data: vec![255; 4],
            },
            &fixture,
            PageSize {
                width: 100.0,
                height: 200.0,
            },
        )?;
        let program = state.temp.path().join("fake-scanimage");
        fs::write(&program, "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --output-file) shift; output=$1;; esac; shift; done\ncp \"$SCAN_FIXTURE\" \"$output\"\n")?;
        // Pass the fixture as a literal argument through a script-specific variable,
        // without changing the process-wide environment used by parallel tests.
        let script = format!(
            "#!/bin/sh\nSCAN_FIXTURE={}\n{}",
            shell_words::quote(&fixture.to_string_lossy()),
            fs::read_to_string(&program)?
        );
        fs::write(&program, script)?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        let options = scanning::Options {
            device: Some("test".into()),
            ..Default::default()
        };
        let job = scanning::Job::start_with(&options, state.temp.path(), &program, None)?;
        state.scan = Some(scanning::Session {
            options: options.clone(),
            job: Some(job),
            progress: Default::default(),
            error: None,
            cancelling: false,
            pages_added: 0,
            ready: false,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !state.scan.as_ref().unwrap().ready {
            handle_scan_input(&mut state, Input::Unknown)?;
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state.page_count, 1);
        assert!(state.dirty);
        let snapshot = state.current_snapshot;
        let failed_program = state.temp.path().join("failed-scanimage");
        fs::write(
            &failed_program,
            "#!/bin/sh\nprintf 'Scanner is busy\\n' >&2\nexit 3\n",
        )?;
        fs::set_permissions(&failed_program, fs::Permissions::from_mode(0o700))?;
        let job = scanning::Job::start_with(&options, state.temp.path(), &failed_program, None)?;
        state.scan = Some(scanning::Session {
            options,
            job: Some(job),
            progress: Default::default(),
            error: None,
            cancelling: false,
            pages_added: 0,
            ready: false,
        });
        while state.scan.as_ref().unwrap().error.is_none() {
            handle_scan_input(&mut state, Input::Unknown)?;
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state.current_snapshot, snapshot);
        assert_eq!(state.page_count, 1);
        handle_scan_input(&mut state, Input::Esc)?;
        assert!(state.scan.is_none());
        undo(&mut state)?;
        assert_eq!(state.page_count, 0);
        redo(&mut state)?;
        assert_eq!(state.page_count, 1);
        Ok(())
    }

    #[test]
    fn shortcuts_work_with_cyrillic_but_text_input_is_unchanged() -> Result<()> {
        for c in ['p', 'з'] {
            assert!(matches!(
                input_from_event(Event::Key(KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::CONTROL
                ))),
                Input::Ctrl('p')
            ));
        }
        assert!(matches!(
            actions::normal_input(Input::Char('К')),
            Input::Char('R')
        ));
        assert!(matches!(
            actions::normal_input(Input::Char('і')),
            Input::Char('s')
        ));
        let mut state = empty_state()?;
        state.command = Some(actions::LineInput::new("add "));
        for c in "фото.pdf".chars() {
            handle_input(&mut state, "kitty", Input::Char(c))?;
        }
        assert_eq!(state.command.as_ref().unwrap().text, "add фото.pdf");
        let repeated =
            KeyEvent::new_with_kind(KeyCode::Char('з'), KeyModifiers::ALT, KeyEventKind::Repeat);
        assert!(matches!(
            input_from_event(Event::Key(repeated)),
            Input::Unknown
        ));
        Ok(())
    }

    #[test]
    fn searchable_actions_execute_and_prefill_commands() -> Result<()> {
        let mut state = empty_state()?;
        state.page_count = 4;
        handle_input(&mut state, "kitty", Input::Function(1))?;
        for c in "select all".chars() {
            handle_input(&mut state, "kitty", Input::Char(c))?;
        }
        handle_input(&mut state, "kitty", Input::Enter)?;
        assert_eq!(state.selected, BTreeSet::from([0, 1, 2, 3]));
        assert!(state.help.is_none());
        handle_input(&mut state, "kitty", Input::Function(1))?;
        for c in "page range".chars() {
            handle_input(&mut state, "kitty", Input::Char(c))?;
        }
        handle_input(&mut state, "kitty", Input::Enter)?;
        assert_eq!(state.command.as_ref().unwrap().text, "select ");
        for c in "1,3".chars() {
            handle_input(&mut state, "kitty", Input::Char(c))?;
        }
        handle_input(&mut state, "kitty", Input::Enter)?;
        assert_eq!(state.selected, BTreeSet::from([0, 2]));
        Ok(())
    }

    #[test]
    fn print_page_sets_follow_current_edited_document() -> Result<()> {
        let mut state = empty_state()?;
        state.page_count = 5;
        state.page = 3;
        state.selected = BTreeSet::from([0, 2]);
        let mut options = printing::PrintOptions::default();
        assert_eq!(print_pages(&options, &state)?, [0, 1, 2, 3, 4]);
        options.pages = "current".into();
        assert_eq!(print_pages(&options, &state)?, [3]);
        options.pages = "selected".into();
        assert_eq!(print_pages(&options, &state)?, [0, 2]);
        options.pages = "1,3-5".into();
        assert_eq!(print_pages(&options, &state)?, [0, 2, 3, 4]);
        options.pages = "0".into();
        assert!(print_pages(&options, &state).is_err());
        options.pages = "6".into();
        assert!(print_pages(&options, &state).is_err());
        options.pages = "selected".into();
        state.selected.clear();
        assert!(print_pages(&options, &state).is_err());
        Ok(())
    }

    #[test]
    fn unsaved_changes_require_save_or_explicit_discard() -> Result<()> {
        let mut state = empty_state()?;
        state.dirty = true;
        assert!(command_line(&mut state, "kitty", ":q").is_err());
        assert!(matches!(
            command_line(&mut state, "kitty", ":q!")?,
            UiAction::Quit
        ));
        Ok(())
    }

    #[test]
    fn parses_pages_and_ranges() {
        let pages = parse_pages("1,3-5,7", 10).unwrap();
        assert_eq!(pages.into_iter().collect::<Vec<_>>(), vec![0, 2, 3, 4, 6]);
    }

    #[test]
    fn parses_reversed_ranges() {
        let pages = parse_pages("4-2", 5).unwrap();
        assert_eq!(pages.into_iter().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn rejects_out_of_range_pages() {
        assert!(parse_pages("0", 2).is_err());
        assert!(parse_pages("3", 2).is_err());
    }

    #[test]
    fn formats_page_ranges_for_qpdf() {
        assert_eq!(page_range(&[0, 2, 3]), "1,3,4");
    }

    #[test]
    fn detects_images_case_insensitively() {
        assert!(is_image(Path::new("scan.JPG")));
        assert!(!is_image(Path::new("scan.pdf")));
    }

    #[test]
    fn quit_commands_return_to_ui_for_cleanup() -> Result<()> {
        let mut state = State::new(
            Args {
                pdf: None,
                page: 1,
                out: None,
                overwrite: false,
                format: None,
                scan: false,
                scan_dpi: None,
            },
            tempfile::tempdir()?,
        )?;
        for command in [":q", "quit", ":exit", "close"] {
            assert!(matches!(
                command_line(&mut state, "kitty", command)?,
                UiAction::Quit
            ));
        }
        Ok(())
    }

    #[test]
    fn move_places_pages_before_original_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bitmap = Bitmap {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![255; 4],
        };
        let mut parts = Vec::new();
        for width in [40, 80, 120, 160] {
            let path = temp.path().join(format!("page-{width}.pdf"));
            bitmap_to_pdf_with_page_size(
                &bitmap,
                &path,
                PageSize {
                    width: width as f64,
                    height: 200.0,
                },
            )?;
            parts.push(path);
        }
        let input = temp.path().join("pages.pdf");
        concat_pdfs(&parts, &input)?;
        for (selected, target, expected) in [
            (vec![0], 3, vec![80, 40, 120, 160]),
            (vec![3], 2, vec![40, 160, 80, 120]),
            (vec![0, 2], 4, vec![80, 40, 120, 160]),
        ] {
            let mut state = State::new(
                Args {
                    pdf: Some(input.clone()),
                    page: 1,
                    out: None,
                    overwrite: false,
                    format: None,
                    scan: false,
                    scan_dpi: None,
                },
                tempfile::tempdir()?,
            )?;
            state.selected = selected.into_iter().collect();
            move_pages(&mut state, target)?;
            let widths = (1..=4)
                .map(|page| pdf_page_size(state.current_pdf()?, page).map(|size| size.width as u32))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(widths, expected);
            undo(&mut state)?;
            assert_eq!(pdf_page_size(state.current_pdf()?, 1)?.width, 40.0);
            assert!(move_pages(&mut state, 0).is_err());
            assert!(move_pages(&mut state, 5).is_err());
        }
        Ok(())
    }

    #[test]
    fn command_backed_pdf_helpers_work() -> Result<()> {
        if require_tool("magick").is_err()
            || require_tool("qpdf").is_err()
            || require_tool("pdfinfo").is_err()
        {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let one = temp.path().join("one.pdf");
        let two = temp.path().join("two.pdf");
        run(Command::new("magick")
            .arg("-size")
            .arg("100x140")
            .arg("xc:white")
            .arg(&one))?;
        run(Command::new("magick")
            .arg("-size")
            .arg("100x140")
            .arg("xc:black")
            .arg(&two))?;
        qpdf_check(&one)?;
        qpdf_check(&two)?;

        let joined = temp.path().join("joined.pdf");
        concat_pdfs(&[one.clone(), two], &joined)?;
        assert_eq!(page_count(&joined)?, 2);

        let reordered = temp.path().join("reordered.pdf");
        rebuild_from_ranges(&joined, &[1, 0], &reordered)?;
        qpdf_check(&reordered)?;
        assert_eq!(page_count(&reordered)?, 2);
        Ok(())
    }

    #[test]
    fn add_files_appends_images_and_pdfs() -> Result<()> {
        if require_tool("magick").is_err()
            || require_tool("qpdf").is_err()
            || require_tool("pdfinfo").is_err()
        {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let png = temp.path().join("scan.png");
        let jpg = temp.path().join("photo.jpg");
        let pdf = temp.path().join("doc.pdf");
        run(Command::new("magick")
            .arg("-size")
            .arg("120x160")
            .arg("xc:white")
            .arg(&png))?;
        run(Command::new("magick")
            .arg("-size")
            .arg("120x160")
            .arg("xc:black")
            .arg(&jpg))?;
        run(Command::new("magick")
            .arg("-size")
            .arg("120x160")
            .arg("xc:gray")
            .arg(&pdf))?;

        let args = Args {
            pdf: None,
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        add_files(
            &mut state,
            &[
                png.display().to_string(),
                jpg.display().to_string(),
                pdf.display().to_string(),
            ],
        )?;

        assert_eq!(state.page_count, 3);
        qpdf_check(state.current_pdf()?)?;
        Ok(())
    }

    #[test]
    fn image_import_preserves_source_pixel_dimensions() -> Result<()> {
        if require_tool("magick").is_err()
            || require_tool("qpdf").is_err()
            || require_tool("pdfimages").is_err()
        {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let image = temp.path().join("photo.jpg");
        let pdf = temp.path().join("photo.pdf");
        run(Command::new("magick")
            .arg("-size")
            .arg("1800x2400")
            .arg("gradient:")
            .arg(&image))?;

        image_to_pdf(&image, &pdf)?;

        let images = pdfimages_list(&pdf)?;
        assert!(images.contains("1800  2400"), "{images}");
        assert!(!images.contains("595   842"), "{images}");
        Ok(())
    }

    #[test]
    fn rejects_unsupported_add_file_type() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let note = temp.path().join("note.txt");
        fs::write(&note, b"not a pdf or image")?;

        let args = Args {
            pdf: None,
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;

        let err = add_files(&mut state, &[note.display().to_string()]).unwrap_err();
        assert!(err.to_string().contains("unsupported add file type"));
        Ok(())
    }

    #[test]
    fn save_as_writes_pdf_and_updates_output_path() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: None,
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        let renamed = temp.path().join("renamed.pdf");

        command_line(
            &mut state,
            "kitty",
            &format!(":save-as {}", renamed.display()),
        )?;

        assert_eq!(state.out, renamed);
        assert!(!state.overwrite);
        assert!(!state.dirty);
        qpdf_check(&state.out)?;
        assert_eq!(page_count(&state.out)?, 1);
        Ok(())
    }

    #[test]
    fn save_as_adds_pdf_extension_when_omitted() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: None,
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        let raw = temp.path().join("renamed");

        command_line(&mut state, "kitty", &format!(":save-as {}", raw.display()))?;

        assert_eq!(state.out, temp.path().join("renamed.pdf"));
        qpdf_check(&state.out)?;
        Ok(())
    }

    #[test]
    fn save_command_accepts_name_and_directory() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        let dir = temp.path().join("saved");

        command_line(
            &mut state,
            "kitty",
            &format!(":save 'Mieterhohung 2026' {}", dir.display()),
        )?;

        let expected = dir.join("Mieterhohung 2026.pdf");
        assert_eq!(state.out, expected);
        assert!(state.out.exists());
        assert!(!state.dirty);
        qpdf_check(&state.out)?;
        Ok(())
    }

    #[test]
    fn save_command_accepts_bare_name_in_current_output_dir() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("nested").join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;

        command_line(&mut state, "kitty", ":save examplename")?;

        assert_eq!(
            state.out,
            temp.path().join("nested").join("examplename.pdf")
        );
        assert!(state.out.exists());
        qpdf_check(&state.out)?;
        Ok(())
    }

    #[test]
    fn saved_pdf_uses_minimum_supported_pdf_version() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;

        save(&mut state, None)?;

        assert_eq!(pdf_version(&state.out)?, SAVED_PDF_MIN_VERSION);
        Ok(())
    }

    #[test]
    fn rename_moves_saved_output_file() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        save(&mut state, None)?;
        let old = state.out.clone();

        command_line(&mut state, "kitty", ":rename renamed")?;

        assert!(!old.exists());
        assert_eq!(state.out, temp.path().join("renamed.pdf"));
        assert!(state.out.exists());
        qpdf_check(&state.out)?;
        Ok(())
    }

    #[test]
    fn deleting_only_page_returns_to_empty_document_and_can_undo() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;

        delete_pages(&mut state, true)?;

        assert_eq!(state.page_count, 0);
        assert!(state.current_pdf.is_none());
        assert!(state.dirty);
        assert!(save(&mut state, None).is_err());

        undo(&mut state)?;
        assert_eq!(state.page_count, 1);
        qpdf_check(state.current_pdf()?)?;
        Ok(())
    }

    #[test]
    fn save_to_existing_path_can_be_cancelled_without_changing_output_path() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let existing = temp.path().join("existing.pdf");
        let bitmap = solid_bitmap(320, 420, [255, 255, 255, 255]);
        bitmap_to_pdf(&bitmap, &source)?;
        bitmap_to_pdf(&bitmap, &existing)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        let original_out = state.out.clone();

        state.message = format!("save cancelled: {}", existing.display());

        assert_eq!(state.out, original_out);
        assert!(state.message.contains("save cancelled"));
        Ok(())
    }

    #[test]
    fn compression_preview_reduces_large_raster_pdf() -> Result<()> {
        if require_tool("qpdf").is_err() || require_tool("pdfinfo").is_err() {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("large.pdf");
        let bitmap = patterned_bitmap(1800, 2400);
        bitmap_to_pdf(&bitmap, &source)?;
        let before = file_size(&source)?;

        let args = Args {
            pdf: Some(source),
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;
        compress_pages(&mut state, "preview", true)?;
        let after = file_size(state.current_pdf()?)?;

        assert!(after < before, "{after} should be smaller than {before}");
        assert!(state.message.contains("smaller"));
        qpdf_check(state.current_pdf()?)?;
        Ok(())
    }

    #[test]
    fn crop_updates_page_box_without_rasterizing_images() -> Result<()> {
        if require_tool("qpdf").is_err()
            || require_tool("pdfinfo").is_err()
            || require_tool("pdfimages").is_err()
        {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pdf");
        let bitmap = patterned_bitmap(1800, 2400);
        bitmap_to_pdf(&bitmap, &source)?;

        let before_size = pdf_page_size(&source, 1)?;
        let before_images = pdfimages_list(&source)?;
        assert!(before_images.contains("1800  2400"));

        crop_page_boxes(&source, 1, [100, 80, 120, 90], 900, 1200)?;

        let after_size = pdf_page_size(&source, 1)?;
        let after_images = pdfimages_list(&source)?;
        assert!(after_size.width < before_size.width);
        assert!(after_size.height < before_size.height);
        assert!(after_images.contains("1800  2400"));
        qpdf_check(&source)?;
        Ok(())
    }

    #[test]
    fn mirror_uses_scan_resolution_instead_of_preview_size() -> Result<()> {
        if require_tool("qpdf").is_err()
            || require_tool("pdfinfo").is_err()
            || require_tool("pdfimages").is_err()
            || require_tool("magick").is_err()
        {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let image = temp.path().join("scan.jpg");
        let source = temp.path().join("source.pdf");
        let mirrored = temp.path().join("mirrored.pdf");
        run(Command::new("magick")
            .arg("-size")
            .arg("2550x3508")
            .arg("gradient:")
            .arg(&image))?;
        image_to_pdf(&image, &source)?;

        mirror_page_to_pdf(&source, 1, &mirrored)?;

        let images = pdfimages_list(&mirrored)?;
        assert!(images.contains("2550  3508"), "{images}");
        assert!(!images.contains("1600  2200"), "{images}");
        Ok(())
    }

    #[test]
    fn ls_lists_addable_files_and_directories() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("doc.pdf"), b"not checked here")?;
        fs::write(temp.path().join("photo.JPG"), b"not checked here")?;
        fs::write(temp.path().join("notes.txt"), b"not addable")?;
        fs::create_dir(temp.path().join("folder"))?;

        let args = Args {
            pdf: None,
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;

        command_line(
            &mut state,
            "kitty",
            &format!(":ls {}", temp.path().display()),
        )?;

        let listing = state.listing_page.as_ref().expect("listing page");
        let labels = listing
            .entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"folder/"));
        assert!(labels.contains(&"doc.pdf"));
        assert!(labels.contains(&"photo.JPG"));
        assert!(!labels.contains(&"notes.txt"));

        let doc_idx = state
            .listing_page
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .position(|entry| entry.label == "doc.pdf")
            .unwrap();
        state.listing_page.as_mut().unwrap().selected = doc_idx;
        handle_listing_input(&mut state, "kitty", Input::Char(' '))?;
        assert!(state
            .listing_page
            .as_ref()
            .unwrap()
            .marked
            .contains(&doc_idx));
        Ok(())
    }

    #[test]
    fn add_directory_opens_picker() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("doc.pdf"), b"not checked here")?;

        let args = Args {
            pdf: None,
            page: 1,
            out: Some(temp.path().join("out.pdf")),
            overwrite: false,
            format: Some("kitty".into()),
            scan: false,
            scan_dpi: None,
        };
        let state_temp = tempfile::tempdir()?;
        let mut state = State::new(args, state_temp)?;

        command_line(
            &mut state,
            "kitty",
            &format!(":add {}", temp.path().display()),
        )?;

        let listing = state.listing_page.as_ref().expect("listing page");
        assert!(listing.entries.iter().any(|entry| entry.label == "doc.pdf"));
        Ok(())
    }

    #[test]
    fn parses_new_compression_presets_and_legacy_aliases() -> Result<()> {
        assert_eq!(compression_limits("strong")?.2, "strong");
        assert_eq!(compression_limits("preview")?.2, "strong");
        assert_eq!(compression_limits("screen")?.2, "strong");
        assert_eq!(compression_limits("normal")?.2, "normal");
        assert_eq!(compression_limits("email")?.2, "normal");
        assert_eq!(compression_limits("medium")?.2, "normal");
        assert_eq!(compression_limits("light")?.2, "light");
        assert_eq!(compression_limits("archive")?.2, "light");
        assert_eq!(compression_limits("print")?.2, "light");
        assert!(compression_limits("tiny").is_err());
        Ok(())
    }

    fn pdfimages_list(path: &Path) -> Result<String> {
        let output = Command::new("pdfimages")
            .arg("-list")
            .arg(path)
            .output()
            .with_context(|| "run pdfimages -list")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn pdf_version(path: &Path) -> Result<String> {
        let output = Command::new("pdfinfo")
            .arg(path)
            .output()
            .with_context(|| "run pdfinfo")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(raw) = line.strip_prefix("PDF version:") {
                return Ok(raw.trim().to_string());
            }
        }
        bail!("pdfinfo did not report PDF version");
    }

    fn solid_bitmap(width: usize, height: usize, rgba: [u8; 4]) -> Bitmap {
        let stride = width * 4;
        let mut data = vec![0_u8; stride * height];
        for px in data.as_chunks_mut::<4>().0 {
            px[0] = rgba[2];
            px[1] = rgba[1];
            px[2] = rgba[0];
            px[3] = rgba[3];
        }
        Bitmap {
            width,
            height,
            stride,
            data,
        }
    }

    fn patterned_bitmap(width: usize, height: usize) -> Bitmap {
        let stride = width * 4;
        let mut data = vec![0_u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let idx = y * stride + x * 4;
                data[idx] = ((x * 17 + y * 3) % 251) as u8;
                data[idx + 1] = ((x * 5 + y * 19) % 253) as u8;
                data[idx + 2] = ((x * 11 + y * 7) % 247) as u8;
                data[idx + 3] = 255;
            }
        }
        Bitmap {
            width,
            height,
            stride,
            data,
        }
    }
}
