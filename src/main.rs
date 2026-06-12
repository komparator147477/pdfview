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
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use serde_json::Value;
use tempfile::{Builder, TempDir};

const IMAGE_IMPORT_DPI: &str = "300";
const RASTER_EDIT_DPI: f64 = 300.0;
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
#[command(about = "Terminal PDF viewer/editor for scanned PDF cleanup.")]
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
    show_help_page: bool,
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
            show_help_page: false,
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
    Unknown,
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            terminal::enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
            Ok(Self { active: true })
        } else {
            Ok(Self { active: false })
        }
    }
}

fn enter_scan_screen() -> Result<()> {
    terminal::disable_raw_mode().ok();
    clear_terminal_images("kitty")?;
    execute!(
        io::stdout(),
        Show,
        MoveTo(0, 0),
        Clear(ClearType::All),
        SetForegroundColor(Color::Green)
    )?;
    println!("pdfview scan");
    execute!(io::stdout(), ResetColor)?;
    println!("Scanner output follows. Password/progress prompts are handled here.");
    println!();
    io::stdout().flush()?;
    Ok(())
}

fn leave_scan_screen() -> Result<()> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), Hide, MoveTo(0, 0), Clear(ClearType::All))?;
    }
    Ok(())
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(
                io::stdout(),
                Clear(ClearType::All),
                DisableMouseCapture,
                Show,
                LeaveAlternateScreen
            );
            let _ = io::stdout().write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b_Ga=d,d=a,q=2;\x1b\\");
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    require_tool("qpdf")?;
    require_tool("pdfinfo")?;

    let fmt = args.format.clone().unwrap_or_else(native_format);
    let temp = tempfile::tempdir()?;
    let mut state = State::new(args, temp)?;
    run_ui(&mut state, &fmt)
}

fn run_ui(state: &mut State, fmt: &str) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut redraw = true;
    loop {
        if redraw {
            if let Err(err) = show(state, fmt) {
                state.message = err.to_string();
            }
            redraw = false;
        }
        if state.message.is_empty() {
            redraw = current_page_render_is_ready(state, fmt);
        }
        if redraw {
            continue;
        }
        match read_input()? {
            input if handle_navigation_input(state, input) => redraw = true,
            input => match handle_action_input(state, fmt, input)? {
                UiAction::Redraw => redraw = true,
                UiAction::Noop => redraw = false,
                UiAction::Quit => return Ok(()),
            },
        }
    }
}

enum UiAction {
    Redraw,
    Noop,
    Quit,
}

fn handle_navigation_input(state: &mut State, input: Input) -> bool {
    if state.crop_preview.is_some() {
        return false;
    }
    if state.listing_page.is_some() {
        return false;
    }
    if (state.show_tree_page || state.show_help_page || state.listing_page.is_some())
        && matches!(
            input,
            Input::Char('n')
                | Input::Char(' ')
                | Input::Right
                | Input::Char('p')
                | Input::Left
                | Input::Char('g')
                | Input::Char('G')
        )
    {
        state.show_tree_page = false;
        state.show_help_page = false;
        state.listing_page = None;
        return true;
    }
    let old_page = state.page;
    match input {
        Input::Up | Input::Down => return false,
        Input::Char('n') | Input::Char(' ') | Input::Right | Input::WheelDown => {
            if state.page_count > 0 {
                state.page = (state.page + 1).min(state.page_count - 1);
            }
        }
        Input::Char('p') | Input::Left | Input::WheelUp => {
            state.page = state.page.saturating_sub(1);
        }
        Input::Char('g') => state.page = 0,
        Input::Char('G') => {
            if state.page_count > 0 {
                state.page = state.page_count - 1;
            }
        }
        _ => return false,
    }
    state.page != old_page
}

fn handle_action_input(state: &mut State, fmt: &str, input: Input) -> Result<UiAction> {
    if state.listing_page.is_some() {
        return handle_listing_input(state, fmt, input);
    }
    if state.show_tree_page {
        return handle_tree_input(state, fmt, input);
    }
    match input {
        Input::Esc
            if state.show_tree_page || state.show_help_page || state.listing_page.is_some() =>
        {
            state.show_tree_page = false;
            state.show_help_page = false;
            state.listing_page = None;
            Ok(UiAction::Redraw)
        }
        Input::CtrlC | Input::Esc | Input::Char('q') | Input::Char('x') => Ok(UiAction::Quit),
        Input::Char('o') => {
            let result = go_to_page_prompt(state);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('m') => {
            if state.page_count > 0 {
                if !state.selected.insert(state.page) {
                    state.selected.remove(&state.page);
                    state.add_history(format!("unselected: page {}", state.page + 1));
                } else {
                    state.add_history(format!("selected: page {}", state.page + 1));
                }
            }
            Ok(UiAction::Redraw)
        }
        Input::Char('r') => {
            let result = rotate(state, 90, true);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('R') => {
            let result = rotate(state, -90, true);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('M') => {
            let result = mirror(state, true);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('d') => {
            let result = delete_pages(state, true);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('<') => {
            let result = move_pages_by(state, -1);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('>') => {
            let result = move_pages_by(state, 1);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('c') => {
            let result = crop_pages(state, fmt, true);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('S') => {
            let result = scan_pages(state, None);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('u') | Input::CtrlZ => {
            let result = undo(state);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('U') => {
            let result = redo(state);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char('s') => {
            let result = save(state, None);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char(':') => {
            let line = prompt(":")?;
            let result = command_line(state, fmt, &line);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        _ => Ok(UiAction::Noop),
    }
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
                {
                    if !listing.marked.insert(listing.selected) {
                        listing.marked.remove(&listing.selected);
                    }
                }
            }
            Ok(UiAction::Redraw)
        }
        Input::Enter => {
            let result = activate_listing_selection(state);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        Input::Char(':') => {
            let line = prompt(":")?;
            let result = command_line(state, fmt, &line);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
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
        Input::Char(':') => {
            let line = prompt(":")?;
            let result = command_line(state, fmt, &line);
            handle_result(state, result);
            Ok(UiAction::Redraw)
        }
        _ => Ok(UiAction::Noop),
    }
}

fn current_page_render_is_ready(state: &State, fmt: &str) -> bool {
    if state.show_tree_page || state.show_help_page || state.listing_page.is_some() {
        return false;
    }
    let (cols, rows) = terminal::size().unwrap_or((100, 40));
    let footer_lines = if state.crop_preview.is_some() {
        2 + usize::from(!state.history.is_empty())
    } else {
        3 + usize::from(!state.history.is_empty())
    };
    let footer_start = rows.saturating_sub(footer_lines as u16).max(3);
    let image_rows = footer_start.saturating_sub(2).max(5);
    let key = RenderKey {
        snapshot: state.current_snapshot,
        page: state.page,
        cols,
        rows: image_rows,
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

fn read_input() -> Result<Input> {
    if !io::stdin().is_terminal() {
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        return Ok(Input::Char('q'));
    }
    loop {
        if event::poll(Duration::from_millis(500))? {
            return match event::read()? {
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => Ok(Input::CtrlC),
                Event::Key(KeyEvent {
                    code: KeyCode::Char('z'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => Ok(Input::CtrlZ),
                Event::Key(KeyEvent {
                    code: KeyCode::Char(c),
                    ..
                }) => Ok(Input::Char(c)),
                Event::Key(KeyEvent {
                    code: KeyCode::Enter,
                    ..
                }) => Ok(Input::Enter),
                Event::Key(KeyEvent {
                    code: KeyCode::Tab, ..
                }) => Ok(Input::Tab),
                Event::Key(KeyEvent {
                    code: KeyCode::Esc, ..
                }) => Ok(Input::Esc),
                Event::Key(KeyEvent {
                    code: KeyCode::Left,
                    ..
                }) => Ok(Input::Left),
                Event::Key(KeyEvent {
                    code: KeyCode::Right,
                    ..
                }) => Ok(Input::Right),
                Event::Key(KeyEvent {
                    code: KeyCode::Up, ..
                }) => Ok(Input::Up),
                Event::Key(KeyEvent {
                    code: KeyCode::Down,
                    ..
                }) => Ok(Input::Down),
                Event::Key(KeyEvent {
                    code: KeyCode::Backspace,
                    ..
                }) => Ok(Input::Backspace),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => Ok(Input::WheelDown),
                    MouseEventKind::ScrollUp => Ok(Input::WheelUp),
                    _ => Ok(Input::Unknown),
                },
                _ => Ok(Input::Unknown),
            };
        }
    }
}

fn show(state: &mut State, fmt: &str) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((100, 40));
    let dirty = if state.dirty { "*" } else { "-" };
    if state.page_count == 0 {
        let footer_lines = 2 + usize::from(!state.history.is_empty());
        let footer_start = rows.saturating_sub(footer_lines as u16).max(3);
        let page_rows = footer_start.saturating_sub(1).max(1);
        begin_sync_update()?;
        clear_terminal_images(fmt)?;
        execute!(io::stdout(), MoveTo(0, 0), Clear(ClearType::All))?;
        print!(
            "{}",
            pad_to_cols(
                &format!(
                    "pdfview: empty document | selected 0 | {dirty} | save -> {}",
                    state.out.display()
                ),
                cols
            )
        );
        execute!(io::stdout(), MoveTo(0, 1))?;
        if state.show_tree_page {
            print_tree_page(state, cols, page_rows)?;
        } else if state.show_help_page {
            print_help_page(cols, page_rows)?;
        } else if let Some(listing) = &state.listing_page {
            print_listing_page(listing, cols, page_rows)?;
        }
        let footer = if state.listing_page.is_some() {
            "List: Up/Down move, Space mark, Enter add/open, Esc close | :add FILE still works"
        } else if state.show_tree_page {
            "Tree: Up/Down move, Enter restore, Esc close | :goto N still works"
        } else {
            "New: S scan, :add files, :ls files | Help: :help | Undo: u/^Z, U, :tree, :goto N | File: s save, q quit"
        };
        print_footer(state, cols, rows, footer)?;
        end_sync_update()?;
        return Ok(());
    }

    let footer_lines = 3 + usize::from(!state.history.is_empty());
    let footer_start = rows.saturating_sub(footer_lines as u16).max(3);
    let image_rows = footer_start.saturating_sub(2).max(5);
    let rendered = if state.show_tree_page || state.show_help_page || state.listing_page.is_some() {
        Vec::new()
    } else {
        rendered_page_output(state, fmt, cols, image_rows)?
    };

    begin_sync_update()?;
    clear_terminal_images(fmt)?;
    execute!(io::stdout(), MoveTo(0, 0), Clear(ClearType::All))?;
    let title = state
        .original
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(OsStr::to_str)
        .unwrap_or("new document");
    print_header(state, title, dirty, cols)?;
    if let Some(preview) = state.crop_preview {
        print_crop_controls(state, preview, footer_start, cols)?;
        print_history(state, footer_start + 2, cols, rows)?;
    } else {
        let default_status = if state.listing_page.is_some() {
            "List: Up/Down move, Space mark, Enter add/open, Esc close | :add FILE still works"
        } else if state.show_tree_page {
            "Tree: Up/Down move, Enter restore, Esc close | :goto N still works"
        } else {
            "Move: n/p, g/G, o go to | Select: m toggle/unselect, :select, :unselect, :clear"
        };
        let status = take_message(state, default_status);
        print_status_line(footer_start, &status, cols)?;
        print_legend_line(
            footer_start + 1,
            cols,
            &[
                (
                    "Edit",
                    "r/R rotate, M mirror, d delete, :move, c crop, :compress",
                    Color::Yellow,
                ),
                ("Order", "</> move pages", Color::Blue),
                ("Add", "S scan, :add", Color::Green),
            ],
        )?;
        print_legend_line(
            footer_start + 2,
            cols,
            &[
                ("Undo", "u/^Z, U redo, :tree/:history, :goto N", Color::Cyan),
                ("Help", ":help", Color::Green),
                ("File", "s save, q/x quit", Color::Magenta),
            ],
        )?;
        print_history(state, footer_start + 3, cols, rows)?;
    }
    execute!(io::stdout(), MoveTo(0, 1))?;
    if state.show_tree_page {
        print_tree_page(state, cols, image_rows)?;
    } else if state.show_help_page {
        print_help_page(cols, image_rows)?;
    } else if let Some(listing) = &state.listing_page {
        print_listing_page(listing, cols, image_rows)?;
    } else {
        io::stdout().write_all(&rendered)?;
    }
    end_sync_update()?;
    Ok(())
}

fn print_footer(state: &mut State, cols: u16, rows: u16, default: &str) -> Result<()> {
    let status = take_message(state, default);
    print_status_line(rows.saturating_sub(2), &status, cols)?;
    print_history(state, rows.saturating_sub(1), cols, rows)?;
    io::stdout().flush()?;
    Ok(())
}

fn print_status_line(row: u16, text: &str, cols: u16) -> Result<()> {
    execute!(io::stdout(), MoveTo(0, row))?;
    if let Some((message, rest)) = text.split_once(" || ") {
        print_colored_segment("Status", Color::Green, cols)?;
        print_colored_segment(": ", Color::DarkGrey, cols)?;
        print_colored_segment(message, Color::Yellow, cols)?;
        print_colored_segment(" | ", Color::DarkGrey, cols)?;
        print_colored_segment(rest, Color::White, cols)?;
    } else if let Some(rest) = text.strip_prefix("Move: ") {
        print_colored_segment("Move", Color::Blue, cols)?;
        print_colored_segment(": ", Color::DarkGrey, cols)?;
        print_colored_segment(rest, Color::White, cols)?;
    } else {
        print_colored_segment("Status", Color::Green, cols)?;
        print_colored_segment(": ", Color::DarkGrey, cols)?;
        print_colored_segment(text, Color::White, cols)?;
    }
    execute!(io::stdout(), ResetColor)?;
    clear_to_end_of_line(cols)?;
    Ok(())
}

fn print_legend_line(row: u16, cols: u16, groups: &[(&str, &str, Color)]) -> Result<()> {
    execute!(io::stdout(), MoveTo(0, row))?;
    for (idx, (label, body, color)) in groups.iter().enumerate() {
        if idx > 0 {
            print_colored_segment(" | ", Color::DarkGrey, cols)?;
        }
        print_colored_segment(label, *color, cols)?;
        print_colored_segment(": ", Color::DarkGrey, cols)?;
        print_colored_segment(body, Color::White, cols)?;
    }
    execute!(io::stdout(), ResetColor)?;
    clear_to_end_of_line(cols)?;
    Ok(())
}

fn print_crop_controls(state: &mut State, preview: CropPreview, row: u16, cols: u16) -> Result<()> {
    let names = ["left", "top", "right", "bottom"];
    let status = take_message(state, "Crop: arrows/hjkl +5, HJKL +50, -5, _ -50, Tab edge");
    print_status_line(row, &status, cols)?;
    execute!(io::stdout(), MoveTo(0, row + 1))?;
    print_colored_segment("Crop", Color::Yellow, cols)?;
    print_colored_segment(": ", Color::DarkGrey, cols)?;
    for (idx, name) in names.iter().enumerate() {
        if idx > 0 {
            print_colored_segment(" | ", Color::DarkGrey, cols)?;
        }
        let color = if idx == preview.active_edge {
            Color::Red
        } else {
            Color::White
        };
        print_colored_segment(&format!("{name} {}", preview.margins[idx]), color, cols)?;
    }
    execute!(io::stdout(), ResetColor)?;
    clear_to_end_of_line(cols)?;
    Ok(())
}

fn print_header(state: &State, title: &str, dirty: &str, cols: u16) -> Result<()> {
    let selected_here = state.selected.contains(&state.page);
    let page_label = if selected_here {
        format!("page {}/{} SELECTED", state.page + 1, state.page_count)
    } else {
        format!("page {}/{}", state.page + 1, state.page_count)
    };
    let selection = if state.selected.is_empty() {
        "selected 0".to_string()
    } else {
        format!(
            "selected {} [{}]",
            state.selected.len(),
            page_ranges_label(state.selected.iter().copied())
        )
    };
    execute!(io::stdout(), MoveTo(0, 0), SetForegroundColor(Color::White))?;
    print_colored_segment(title, Color::White, cols)?;
    print_colored_segment(" | ", Color::DarkGrey, cols)?;
    print_colored_segment(
        &page_label,
        if selected_here {
            Color::Yellow
        } else {
            Color::White
        },
        cols,
    )?;
    print_colored_segment(" | ", Color::DarkGrey, cols)?;
    print_colored_segment(
        &selection,
        if state.selected.is_empty() {
            Color::DarkGrey
        } else {
            Color::Yellow
        },
        cols,
    )?;
    print_colored_segment(" | ", Color::DarkGrey, cols)?;
    print_colored_segment(
        dirty,
        if state.dirty {
            Color::Red
        } else {
            Color::DarkGrey
        },
        cols,
    )?;
    print_colored_segment(" | save -> ", Color::DarkGrey, cols)?;
    print_colored_segment(&state.out.display().to_string(), Color::White, cols)?;
    execute!(io::stdout(), ResetColor)?;
    clear_to_end_of_line(cols)?;
    Ok(())
}

fn print_colored_segment(text: &str, color: Color, cols: u16) -> Result<()> {
    let (x, _) = crossterm::cursor::position().unwrap_or((cols, 0));
    if x >= cols {
        return Ok(());
    }
    execute!(io::stdout(), SetForegroundColor(color))?;
    print!("{}", truncate(text, cols - x));
    Ok(())
}

fn clear_to_end_of_line(cols: u16) -> Result<()> {
    let (x, _) = crossterm::cursor::position().unwrap_or((cols, 0));
    if x < cols {
        print!("{}", " ".repeat((cols - x) as usize));
    }
    Ok(())
}

fn print_history(state: &State, mut row: u16, cols: u16, rows: u16) -> Result<()> {
    if !state.history.is_empty() {
        print_recent_line(row, cols, &state.history)?;
        row += 1;
    }
    while row < rows {
        print_at(row, &" ".repeat(cols as usize))?;
        row += 1;
    }
    Ok(())
}

fn print_recent_line(row: u16, cols: u16, history: &[String]) -> Result<()> {
    execute!(io::stdout(), MoveTo(0, row))?;
    print_colored_segment("Recent", Color::DarkGrey, cols)?;
    print_colored_segment(": ", Color::DarkGrey, cols)?;
    for (idx, item) in history.iter().enumerate() {
        if idx > 0 {
            print_colored_segment(" | ", Color::DarkGrey, cols)?;
        }
        print_colored_segment(&history_label(item), history_color(item), cols)?;
    }
    execute!(io::stdout(), ResetColor)?;
    clear_to_end_of_line(cols)?;
    Ok(())
}

fn history_label(item: &str) -> String {
    let max = 48;
    if item.chars().count() <= max {
        item.to_string()
    } else {
        format!(
            "{}...",
            item.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn print_tree_page(state: &State, cols: u16, rows: u16) -> Result<()> {
    let lines = tree_lines(state);
    for row in 0..rows {
        execute!(io::stdout(), MoveTo(0, row + 1))?;
        if let Some((id, line)) = lines.get(row as usize) {
            let selected = row as usize == state.tree_cursor;
            let marker = if selected { "> " } else { "  " };
            let color = if selected {
                Color::Yellow
            } else if *id == state.current_snapshot {
                Color::Yellow
            } else if *id == state.saved_snapshot {
                Color::Green
            } else {
                Color::White
            };
            print_colored_segment(&format!("{marker}{line}"), color, cols)?;
            execute!(io::stdout(), ResetColor)?;
            clear_to_end_of_line(cols)?;
        } else {
            print!("{}", " ".repeat(cols as usize));
        }
    }
    Ok(())
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

fn print_help_page(cols: u16, rows: u16) -> Result<()> {
    let lines = [
        ("pdfview help", Color::Yellow),
        ("", Color::White),
        ("Keys", Color::Cyan),
        ("  n / Right / Space     next page", Color::White),
        ("  p / Left              previous page", Color::White),
        ("  mouse wheel           next / previous page", Color::White),
        ("  g / G                 first / last page", Color::White),
        ("  o                     go to page prompt", Color::White),
        (
            "  m                     toggle current page selection",
            Color::White,
        ),
        ("  r / R                 rotate right / left", Color::White),
        (
            "  M                     mirror selected/current page",
            Color::White,
        ),
        (
            "  d                     delete selected/current page",
            Color::White,
        ),
        (
            "  < / >                 move selected/current pages backward / forward",
            Color::White,
        ),
        (
            "  c                     crop selected/current pages",
            Color::White,
        ),
        (
            "  S                     scan with default DPI",
            Color::White,
        ),
        ("  u / Ctrl-Z / U        undo / redo", Color::White),
        (
            "  s                     save to current output path",
            Color::White,
        ),
        (
            "  q / x / Esc / Ctrl-C  quit, Esc closes help/tree/crop first",
            Color::White,
        ),
        ("", Color::White),
        ("Commands", Color::Cyan),
        (
            "  :save, :w             save to current output path",
            Color::White,
        ),
        (
            "  :save NAME            save as NAME.pdf in current output dir",
            Color::White,
        ),
        ("  :save NAME DIR        save as DIR/NAME.pdf", Color::White),
        (
            "  :save PATH            save to PATH; .pdf is added if omitted",
            Color::White,
        ),
        (
            "  :rename NAME [DIR]    rename/move current output file or target",
            Color::White,
        ),
        (
            "  :save-as PATH         legacy alias for :save PATH",
            Color::White,
        ),
        (
            "  :add [FILE...|DIR]    add files, or open picker when empty/dir",
            Color::White,
        ),
        (
            "  :ls [DIR]             open add picker; Space marks many",
            Color::White,
        ),
        ("  :page N               go to page N", Color::White),
        ("  :select 1,3-5         select page set", Color::White),
        ("  :select all           select every page", Color::White),
        (
            "  :unselect 4-7         remove pages from selection",
            Color::White,
        ),
        ("  :unselect all, :clear clear selection", Color::White),
        (
            "  :rotate, :right       rotate selected/current pages right",
            Color::White,
        ),
        (
            "  :rotate-left, :left   rotate selected/current pages left",
            Color::White,
        ),
        (
            "  :mirror, :flip        mirror selected/current pages",
            Color::White,
        ),
        (
            "  :delete, :del         delete selected/current pages",
            Color::White,
        ),
        (
            "  :crop                 crop selected/current pages",
            Color::White,
        ),
        (
            "  :compress LEVEL       LEVEL: light, normal, strong",
            Color::White,
        ),
        (
            "  :move N               move selection before page N",
            Color::White,
        ),
        (
            "  :back/<, :forward/>   move selection one page backward / forward",
            Color::White,
        ),
        (
            "  :scan [DPI]           scan one or more pages",
            Color::White,
        ),
        (
            "  :tree, :history       show full history tree",
            Color::White,
        ),
        (
            "  :goto N               jump to history node N",
            Color::White,
        ),
        ("  :undo, :redo          undo / redo", Color::White),
        ("  :q, :quit, :exit      quit", Color::White),
        (
            "  :help                 toggle this help page",
            Color::White,
        ),
    ];
    for row in 0..rows {
        execute!(io::stdout(), MoveTo(0, row + 1))?;
        if let Some((line, color)) = lines.get(row as usize) {
            print_colored_segment(line, *color, cols)?;
            execute!(io::stdout(), ResetColor)?;
            clear_to_end_of_line(cols)?;
        } else {
            print!("{}", " ".repeat(cols as usize));
        }
    }
    Ok(())
}

fn print_listing_page(listing: &ListingPage, cols: u16, rows: u16) -> Result<()> {
    for row in 0..rows {
        execute!(io::stdout(), MoveTo(0, row + 1))?;
        if row == 0 {
            print_colored_segment(&listing.title, Color::Yellow, cols)?;
        } else if let Some(entry) = listing.entries.get(row.saturating_sub(1) as usize) {
            let idx = row.saturating_sub(1) as usize;
            let selected = idx == listing.selected && entry.kind != ListingEntryKind::Empty;
            let marked = listing.marked.contains(&idx);
            let color = if selected {
                Color::Yellow
            } else if marked {
                Color::Green
            } else if entry.kind == ListingEntryKind::Directory {
                Color::Cyan
            } else {
                Color::White
            };
            let marker = match (selected, marked) {
                (true, true) => ">*",
                (true, false) => "> ",
                (false, true) => " *",
                (false, false) => "  ",
            };
            print_colored_segment(&format!("{marker}{}", entry.label), color, cols)?;
        } else {
            print!("{}", " ".repeat(cols as usize));
        }
        execute!(io::stdout(), ResetColor)?;
        clear_to_end_of_line(cols)?;
    }
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

fn history_color(item: &str) -> Color {
    if item.starts_with("delete") {
        Color::Red
    } else if item.starts_with("rotate") {
        Color::Yellow
    } else if item.starts_with("mirror") || item.starts_with("crop") {
        Color::Magenta
    } else if item.starts_with("move") {
        Color::Blue
    } else if item.starts_with("selected") {
        Color::Green
    } else if item.starts_with("unselected") {
        Color::DarkYellow
    } else if item.starts_with("save") {
        Color::Cyan
    } else if item.starts_with("undo") || item.starts_with("redo") {
        Color::Blue
    } else {
        Color::White
    }
}

fn take_message(state: &mut State, default: &str) -> String {
    if state.message.is_empty() {
        default.into()
    } else {
        let msg = state.message.clone();
        state.message.clear();
        msg
    }
}

fn print_at(row: u16, text: &str) -> Result<()> {
    execute!(io::stdout(), MoveTo(0, row))?;
    print!("{text}");
    Ok(())
}

fn truncate(text: &str, cols: u16) -> String {
    text.chars().take(cols as usize).collect()
}

fn pad_to_cols(text: &str, cols: u16) -> String {
    let mut out = truncate(text, cols);
    let len = out.chars().count();
    if len < cols as usize {
        out.push_str(&" ".repeat(cols as usize - len));
    }
    out
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
    if let Some(output) = state
        .render_cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned())
    {
        state.last_rendered = Some(output.clone());
        state.last_rendered_key = Some(key);
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

fn prompt(label: &str) -> Result<String> {
    let (cols, rows) = terminal::size().unwrap_or((100, 40));
    let row = rows.saturating_sub(1);
    let mut line = String::new();
    execute!(
        io::stdout(),
        Show,
        MoveTo(0, row),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::Yellow)
    )?;
    loop {
        execute!(
            io::stdout(),
            MoveTo(0, row),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Yellow)
        )?;
        print!("{}", truncate(label, cols));
        execute!(io::stdout(), ResetColor)?;
        let used = label.chars().count().min(cols as usize) as u16;
        if used < cols {
            print!("{}", truncate(&line, cols - used));
        }
        io::stdout().flush()?;
        match event::read()? {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => break,
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) => {
                line.clear();
                break;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) => {
                line.pop();
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                line.push(c);
            }
            _ => {}
        }
    }
    execute!(
        io::stdout(),
        Hide,
        MoveTo(0, row),
        Clear(ClearType::CurrentLine)
    )?;
    Ok(line.trim().to_string())
}

fn confirm(label: &str) -> Result<bool> {
    Ok(prompt(label)?.eq_ignore_ascii_case("y"))
}

fn go_to_page_prompt(state: &mut State) -> Result<()> {
    let raw = prompt("Go to page: ")?;
    go_to_page(state, &raw)
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

fn command_line(state: &mut State, fmt: &str, line: &str) -> Result<()> {
    let parts = shell_words::split(line.trim()).context("parse command")?;
    if parts.is_empty() {
        return Ok(());
    }
    match parts[0].trim_start_matches(':') {
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
            std::process::exit(0);
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
            state.show_help_page = false;
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
            state.show_help_page = !state.show_help_page;
            state.show_tree_page = false;
            state.listing_page = None;
            state.message = if state.show_help_page {
                "help page shown".into()
            } else {
                "help page hidden".into()
            };
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
    }
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
    if target_page_1 > total {
        bail!("target page out of range");
    }
    let target = target_page_1.saturating_sub(1);
    if targets.contains(&target) {
        bail!("target is inside moved selection");
    }
    let target_set: BTreeSet<usize> = targets.iter().copied().collect();
    let remaining: Vec<usize> = (0..total).filter(|p| !target_set.contains(p)).collect();
    let insert_at = target.min(remaining.len());
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
    loop {
        state.crop_preview = Some(CropPreview {
            margins,
            active_edge: edge,
        });
        state.message = format!(
            "crop {} LTRB={},{},{},{} | arrows/hjkl +5, HJKL +50, -5, _ -50, Tab edge",
            names[edge], margins[0], margins[1], margins[2], margins[3]
        );
        show(state, fmt)?;
        match read_input()? {
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
                drain_pending_crop_adjust_inputs()?;
            }
            Input::Right => {
                edge = 2;
                margins[edge] += 5;
                drain_pending_crop_adjust_inputs()?;
            }
            Input::Up => {
                edge = 1;
                margins[edge] += 5;
                drain_pending_crop_adjust_inputs()?;
            }
            Input::Down => {
                edge = 3;
                margins[edge] += 5;
                drain_pending_crop_adjust_inputs()?;
            }
            Input::Tab | Input::Char('\t') => edge = (edge + 1) % 4,
            Input::Char('-') => {
                margins[edge] = margins[edge].saturating_sub(5);
                drain_pending_crop_adjust_inputs()?;
            }
            Input::Char('_') => {
                margins[edge] = margins[edge].saturating_sub(50);
                drain_pending_crop_adjust_inputs()?;
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
                drain_pending_crop_adjust_inputs()?;
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

fn drain_pending_crop_adjust_inputs() -> Result<()> {
    while event::poll(Duration::from_millis(0))? {
        match read_input()? {
            Input::Left
            | Input::Right
            | Input::Up
            | Input::Down
            | Input::Char('h' | 'j' | 'k' | 'l' | 'H' | 'J' | 'K' | 'L' | '-' | '_') => {}
            _ => break,
        }
    }
    Ok(())
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
    let mut added = 0;
    loop {
        let tmp = state.temp.path().join(format!("scan-{added}.pdf"));
        enter_scan_screen()?;
        let scan_result = run_scan_command(&tmp, dpi);
        leave_scan_screen()?;
        scan_result?;
        if !tmp.exists() || fs::metadata(&tmp)?.len() == 0 {
            bail!("scan command finished but did not create a PDF");
        }
        append_pdf_file(state, &tmp, false)?;
        added += 1;
        if !confirm("Scan another page? [y/N] ")? {
            break;
        }
    }
    if added > 0 {
        state.record_snapshot(format!("scan: {added} page{}", plural(added)))?;
        state.message = format!(
            "scanned {added} page{}; showing page {}",
            plural(added),
            state.page + 1
        );
    }
    Ok(())
}

fn run_scan_command(out: &Path, dpi: Option<u32>) -> Result<()> {
    sudo_preflight()?;
    let command = std::env::var("PDFVIEW_SCAN_COMMAND").unwrap_or_else(|_| {
        "source ~/coding/hpscan/hpscan.zsh && hpscan \"$1\" \"$2\"".to_string()
    });
    let mut child = Command::new("zsh")
        .arg("-lc")
        .arg(command)
        .arg("pdfview-scan")
        .arg(out)
        .arg(dpi.map(|dpi| dpi.to_string()).unwrap_or_default())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start scan command")?;
    let status = child.wait().context("wait for scan command")?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "scan command failed with {status}; if sudo rejected the password, stop scanning and test `sudo -v` in a normal terminal"
        );
    }
}

fn sudo_preflight() -> Result<()> {
    println!("Checking sudo authentication before scanner command...");
    io::stdout().flush()?;
    let status = Command::new("sudo")
        .arg("-v")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("run sudo -v before scan")?;
    if status.success() {
        println!("Sudo authentication ok.");
        io::stdout().flush()?;
        Ok(())
    } else {
        bail!(
            "sudo authentication failed before scan; wait for faillock unlock if needed, then test `sudo -v` in a normal terminal"
        )
    }
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
    state.show_help_page = false;
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
            && !confirm(&format!(
                "Rename to {}? File exists; overwrite? [y/N] ",
                out.display()
            ))?
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
    let current = state.current_pdf()?;
    if out.exists()
        && Some(&out) != state.original.as_ref()
        && !confirm(&format!(
            "Save to {}? File exists; overwrite? [y/N] ",
            out.display()
        ))?
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
    fs::copy(current, tmp.path())?;
    qpdf_check(tmp.path())?;
    tmp.persist(&out).map_err(|e| anyhow!(e.error))?;
    state.saved_snapshot = state.current_snapshot;
    state.dirty = false;
    state.message = format!("saved to {}; qpdf ok", out.display());
    state.add_history(format!("save: {}", out.display()));
    Ok(true)
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
        pages.extend((current + 3)..total);
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
    (
        (cols as usize).saturating_mul(10).clamp(240, 2400),
        (rows as usize).saturating_mul(20).clamp(240, 3200),
    )
}

fn crop_preview_pixel_limits(state: &State) -> (usize, usize) {
    let (cols, rows) = terminal::size().unwrap_or((100, 40));
    let footer_lines = 3 + usize::from(!state.history.is_empty());
    let footer_start = rows.saturating_sub(footer_lines as u16).max(3);
    let image_rows = footer_start.saturating_sub(2).max(5);
    render_pixel_limits(cols, image_rows)
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
        for px in row.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    let encoded = base64_encode(&rgba);
    let mut out = Vec::with_capacity(encoded.len() + 128);
    let display_cols = bitmap.width.div_ceil(10).clamp(1, cols as usize);
    let display_rows = bitmap.height.div_ceil(20).clamp(1, rows as usize);
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

    fn solid_bitmap(width: usize, height: usize, rgba: [u8; 4]) -> Bitmap {
        let stride = width * 4;
        let mut data = vec![0_u8; stride * height];
        for px in data.chunks_exact_mut(4) {
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
