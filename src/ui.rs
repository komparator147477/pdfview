use std::io::{self, Write};

use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    execute, terminal,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
        TableState, Wrap,
    },
    Frame, Terminal,
};

use crate::{
    actions::{self, LineInput},
    config::Theme,
    printing, Input, State,
};

pub struct Runtime {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    clear: bool,
    last_image: Option<(crate::RenderKey, Rect, Option<[u32; 4]>)>,
}

impl Runtime {
    fn new() -> Result<Self> {
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(io::stdout()))?,
            clear: true,
            last_image: None,
        })
    }

    pub fn invalidate(&mut self) {
        self.clear = true;
    }

    fn draw(&mut self, state: &mut State, fmt: &str) -> Result<()> {
        let size = terminal::size().unwrap_or((100, 40));
        let viewport = document_area(Rect::new(0, 0, size.0, size.1));
        let modal = state.help.is_some() || state.print_dialog.is_some() || state.scan.is_some();
        let image = if !modal
            && !state.show_tree_page
            && state.listing_page.is_none()
            && state.page_count > 0
            && viewport.width > 1
            && viewport.height > 1
        {
            crate::rendered_page_output(state, fmt, viewport.width, viewport.height)?
        } else {
            Vec::new()
        };
        let image_key = if image.is_empty() {
            None
        } else {
            let key = if state.crop_preview.is_some() {
                crate::RenderKey {
                    snapshot: state.current_snapshot,
                    page: state.page,
                    cols: viewport.width,
                    rows: viewport.height,
                    format: fmt.into(),
                }
            } else {
                state
                    .last_rendered_key
                    .clone()
                    .expect("rendered image has a cache key")
            };
            Some((
                key,
                viewport,
                state.crop_preview.map(|preview| preview.margins),
            ))
        };
        let image_changed = image_key != self.last_image || self.clear;
        crate::begin_sync_update()?;
        if image_changed {
            crate::clear_terminal_images(fmt)?;
        }
        if self.clear {
            self.terminal.clear()?;
            self.clear = false;
        }
        self.terminal.draw(|frame| render(frame, state))?;
        if image_changed && !image.is_empty() {
            let (cols, rows) = image_cells(&image).unwrap_or((viewport.width, viewport.height));
            let x = viewport.x + viewport.width.saturating_sub(cols) / 2;
            let y = viewport.y + viewport.height.saturating_sub(rows) / 2;
            execute!(io::stdout(), SavePosition, MoveTo(x, y))?;
            io::stdout().write_all(&image)?;
            execute!(io::stdout(), RestorePosition)?;
        }
        self.last_image = image_key;
        crate::end_sync_update()
    }
}

pub fn show(state: &mut State, fmt: &str) -> Result<()> {
    let mut runtime = match state.ui.take() {
        Some(runtime) => runtime,
        None => Runtime::new()?,
    };
    let result = runtime.draw(state, fmt);
    state.ui = Some(runtime);
    result
}

fn sections(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area)
        .to_vec()
}

pub fn document_area(area: Rect) -> Rect {
    let parts = sections(area);
    Rect {
        x: parts[1].x.saturating_add(1),
        y: parts[1].y.saturating_add(1),
        width: parts[1].width.saturating_sub(2),
        height: parts[1].height.saturating_sub(2),
    }
}

fn image_cells(image: &[u8]) -> Option<(u16, u16)> {
    let header = std::str::from_utf8(image.get(..image.iter().position(|b| *b == b';')?)?).ok()?;
    let read = |name: &str| {
        header
            .split(',')
            .find_map(|part| part.strip_prefix(name)?.parse().ok())
    };
    Some((read("c=")?, read("r=")?))
}

fn panel<'a>(title: impl Into<Line<'a>>, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(title)
}

fn render(frame: &mut Frame, state: &State) {
    let theme = &state.config.theme;
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.background).fg(theme.foreground)),
        area,
    );
    let parts = sections(area);
    render_header(frame, parts[0], state);
    let title = if state.show_tree_page {
        " History · ↑↓ / jk navigate · Enter restore "
    } else if state.listing_page.is_some() {
        " Append files · Space mark · Enter open/add "
    } else if state.crop_preview.is_some() {
        " Crop · Enter apply · Esc cancel "
    } else {
        " Document "
    };
    frame.render_widget(panel(title, theme), parts[1]);
    let content = document_area(area);
    if state.show_tree_page {
        let lines = crate::tree_lines(state);
        let items: Vec<ListItem> = lines
            .iter()
            .map(|(id, line)| {
                let style = if *id == state.current_snapshot {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else if *id == state.saved_snapshot {
                    Style::default().fg(theme.success)
                } else {
                    Style::default().fg(theme.foreground)
                };
                ListItem::new(line.clone()).style(style)
            })
            .collect();
        let mut selected = ListState::default().with_selected(Some(state.tree_cursor));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(selected_style(theme))
                .highlight_symbol("› "),
            content,
            &mut selected,
        );
    } else if let Some(listing) = &state.listing_page {
        let items: Vec<ListItem> = listing
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let marked = listing.marked.contains(&index);
                let marker = if marked { "● " } else { "  " };
                let style = if marked {
                    Style::default().fg(theme.success)
                } else if entry.kind == crate::ListingEntryKind::Directory {
                    Style::default().fg(theme.accent)
                } else {
                    Style::default().fg(theme.foreground)
                };
                ListItem::new(format!("{marker}{}", entry.label)).style(style)
            })
            .collect();
        let mut selected = ListState::default().with_selected(Some(listing.selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(selected_style(theme))
                .highlight_symbol("› ")
                .block(Block::default().title(
                    Line::from(listing.title.clone()).style(Style::default().fg(theme.muted)),
                )),
            content,
            &mut selected,
        );
    } else if state.page_count == 0 {
        let text = vec![
            Line::from(""),
            Line::from("A new document starts here")
                .style(Style::default().fg(theme.accent).bold()),
            Line::from(""),
            Line::from("O  Browse images and PDFs to append"),
            Line::from("S  Scan new pages"),
            Line::from(""),
            Line::from("F1  Find any action  ·  :  Enter a command")
                .style(Style::default().fg(theme.muted)),
        ];
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Center),
            centered(content, content.width.min(70), 10),
        );
    }
    let status = if let Some(preview) = state.crop_preview {
        format!("Crop [{}] L/T/R/B: {} / {} / {} / {} · arrows/hjkl +5 · HJKL +50 · - / _ reduce · Tab edge", ["left", "top", "right", "bottom"][preview.active_edge], preview.margins[0], preview.margins[1], preview.margins[2], preview.margins[3])
    } else if let Some(prefix) = state.prefix {
        if prefix == 'z' {
            "z → n normal · l light · s strong compression · Esc cancel".into()
        } else {
            "Z → Z save and quit · Q quit without saving · Esc cancel".into()
        }
    } else if !state.message.is_empty() {
        state.message.clone()
    } else {
        format!("Save → {}", state.out.display())
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(theme.muted)),
        parts[2],
    );
    if let Some(command) = &state.command {
        render_input(frame, parts[3], ":", command, theme);
    } else {
        let mode = if state.crop_preview.is_some() {
            " CROP "
        } else if state.show_tree_page {
            " TREE "
        } else if state.listing_page.is_some() {
            " FILES "
        } else {
            " VIEW "
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(mode, selected_style(theme).bold()),
                Span::raw("  "),
                Span::styled("F1", Style::default().fg(theme.accent)),
                Span::raw(" Actions   "),
                Span::styled(":", Style::default().fg(theme.accent)),
                Span::raw(" Command   "),
                Span::styled("Ctrl+P", Style::default().fg(theme.accent)),
                Span::raw(" Print   "),
                Span::styled("s", Style::default().fg(theme.accent)),
                Span::raw(" Save   "),
                Span::styled("u", Style::default().fg(theme.accent)),
                Span::raw(" Undo   "),
                Span::styled("q", Style::default().fg(theme.accent)),
                Span::raw(" Quit"),
            ]))
            .style(Style::default().bg(theme.surface)),
            parts[3],
        );
    }
    if let Some(help) = &state.help {
        render_help(frame, state, help);
    }
    if let Some(dialog) = &state.print_dialog {
        render_print(frame, state, dialog);
    }
    if let Some(session) = &state.scan {
        render_scan(frame, state, session);
    }
}

fn render_scan(frame: &mut Frame, state: &State, session: &crate::scanning::Session) {
    let theme = &state.config.theme;
    let area = centered(frame.area(), 82, 15);
    frame.render_widget(Clear, area);
    let block = panel(" Scan ", theme)
        .border_style(Style::default().fg(if session.error.is_some() {
            theme.error
        } else {
            theme.accent
        }))
        .style(Style::default().bg(theme.background));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(format!(
            " {} DPI · {}",
            session.options.dpi, session.options.mode
        ))
        .style(Style::default().fg(theme.muted)),
        Line::from(""),
    ];
    if session.ready {
        lines.push(
            Line::from(format!(
                " Page {} added · {} page{} scanned",
                state.page + 1,
                session.pages_added,
                if session.pages_added == 1 { "" } else { "s" }
            ))
            .style(Style::default().fg(theme.success).bold()),
        );
        lines.push(Line::from(""));
        lines.push(Line::from(" Place the next sheet on the scanner."));
        lines.push(Line::from(""));
        lines.push(
            Line::from(" Scan another page?  Enter / S — scan next · Esc — done")
                .style(Style::default().fg(theme.accent)),
        );
    } else if let Some(error) = &session.error {
        lines.push(Line::from(" Scan failed").style(Style::default().fg(theme.error).bold()));
        lines.extend(error.lines().map(|line| Line::from(format!(" {line}"))));
        lines.push(Line::from(""));
        lines.push(
            Line::from(" Enter / r retries · Esc returns to document")
                .style(Style::default().fg(theme.accent)),
        );
    } else {
        lines.push(Line::from(if session.cancelling {
            " Cancelling…".into()
        } else {
            format!(" {}", session.progress.message)
        }));
        if let Some(percent) = session.progress.percent {
            lines.push(
                Line::from(format!(" {percent:.0}%"))
                    .style(Style::default().fg(theme.success).bold()),
            );
        }
        lines.push(Line::from(""));
        lines.push(Line::from(" Esc cancels").style(Style::default().fg(theme.accent)));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_header(frame: &mut Frame, area: Rect, state: &State) {
    let theme = &state.config.theme;
    let right = format!(
        " {} / {}  {}{} ",
        if state.page_count == 0 {
            0
        } else {
            state.page + 1
        },
        state.page_count,
        if state.selected.is_empty() {
            String::new()
        } else {
            format!("● {} selected  ", state.selected.len())
        },
        if state.dirty { "● modified" } else { "saved" }
    );
    let columns = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(right.chars().count() as u16),
    ])
    .split(area);
    let name = state
        .original
        .as_ref()
        .and_then(|p| p.file_name())
        .unwrap_or_else(|| std::ffi::OsStr::new("Untitled"));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" PDFVIEW ", selected_style(theme).bold()),
            Span::styled(
                format!("  {}", name.to_string_lossy()),
                Style::default().fg(theme.foreground).bold(),
            ),
        ]))
        .style(Style::default().bg(theme.surface)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(right).alignment(Alignment::Right).style(
            Style::default().bg(theme.surface).fg(if state.dirty {
                theme.warning
            } else {
                theme.muted
            }),
        ),
        columns[1],
    );
}

fn selected_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.selected_foreground)
        .bg(theme.selected_background)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn render_input(frame: &mut Frame, area: Rect, prefix: &str, input: &LineInput, theme: &Theme) {
    let area = area.intersection(frame.area());
    if area.is_empty() {
        return;
    }
    let prefix_width = (Line::from(prefix).width() as u16).min(area.width);
    let style = Style::default().fg(theme.foreground).bg(theme.surface);
    frame.render_widget(
        Paragraph::new(prefix).style(style),
        Rect {
            width: prefix_width,
            ..area
        },
    );
    let remaining = Rect::new(
        area.x + prefix_width,
        area.y,
        area.width - prefix_width,
        area.height,
    );
    let cursor =
        Line::from(input.text.chars().take(input.cursor).collect::<String>()).width() as u16;
    let scroll = cursor.saturating_sub(remaining.width.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(input.text.as_str())
            .style(style)
            .scroll((0, scroll)),
        remaining,
    );
    frame.set_cursor_position((
        remaining.x
            + cursor
                .saturating_sub(scroll)
                .min(remaining.width.saturating_sub(1)),
        remaining.y,
    ));
}

fn render_help(frame: &mut Frame, state: &State, help: &actions::Help) {
    let theme = &state.config.theme;
    let area = centered(
        frame.area(),
        110,
        frame.area().height.saturating_sub(4).max(5),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        panel(" Actions / Help ", theme)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.background)),
        area,
    );
    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);
    render_input(
        frame,
        Rect {
            height: 1,
            ..parts[0]
        },
        "/ ",
        &help.query,
        theme,
    );
    let indices = actions::matching(&help.query.text);
    let rows = indices.iter().map(|&index| {
        let action = &actions::ACTIONS[index];
        Row::new([
            Cell::from(action.keys).style(Style::default().fg(theme.accent)),
            Cell::from(action.name),
            Cell::from(format!(":{}", action.command)).style(Style::default().fg(theme.muted)),
        ])
    });
    let mut selection = TableState::default().with_selected(
        (!indices.is_empty()).then_some(help.selected.min(indices.len().saturating_sub(1))),
    );
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(23),
                Constraint::Min(15),
                Constraint::Length(21),
            ],
        )
        .row_highlight_style(selected_style(theme))
        .highlight_symbol("› ")
        .header(
            Row::new(["Shortcut", "Action", "Command"]).style(Style::default().fg(theme.muted)),
        ),
        parts[1],
        &mut selection,
    );
    let detail = indices
        .get(help.selected)
        .map(|&index| actions::ACTIONS[index].detail)
        .unwrap_or("No matching actions");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(detail).style(Style::default().fg(theme.muted)),
            Line::from("Type to search · ↑↓ / Ctrl+N/P choose · Enter run · Esc close")
                .style(Style::default().fg(theme.accent)),
        ]),
        parts[2],
    );
}

fn render_print(frame: &mut Frame, state: &State, dialog: &printing::Dialog) {
    let theme = &state.config.theme;
    let area = centered(frame.area(), 84, 21);
    frame.render_widget(Clear, area);
    frame.render_widget(
        panel(" Print ", theme)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.background)),
        area,
    );
    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    if inner.height < 15 {
        frame.render_widget(Paragraph::new("Enlarge the terminal to edit print options.\nEnter prints with defaults · Esc cancels").wrap(Wrap { trim: true }), inner);
        return;
    }
    let values = dialog.values();
    for (index, (name, value)) in printing::FIELD_NAMES.iter().zip(&values).enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        let style = if index == dialog.focus {
            selected_style(theme)
        } else {
            Style::default().fg(theme.foreground)
        };
        let columns = Layout::horizontal([Constraint::Length(20), Constraint::Min(1)]).split(row);
        frame.render_widget(Paragraph::new(*name).style(style), columns[0]);
        if index == dialog.focus {
            if let Some(editing) = &dialog.editing {
                render_input(frame, columns[1], "", editing, theme);
                continue;
            }
        }
        frame.render_widget(Paragraph::new(value.clone()).style(style), columns[1]);
    }
    let button = Rect::new(inner.x, inner.y + 11, inner.width, 1);
    frame.render_widget(
        Paragraph::new(if dialog.pending_submit {
            " Preparing printer… Esc cancels "
        } else {
            " Print now · Enter "
        })
        .alignment(Alignment::Center)
        .style(if dialog.focus == printing::SUBMIT {
            selected_style(theme).bold()
        } else {
            Style::default().fg(theme.accent)
        }),
        button,
    );
    let error = if !dialog.error.is_empty() {
        &dialog.error
    } else {
        &dialog.printers.error
    };
    let info = if error.is_empty() && dialog.loading() {
        "Loading printer settings…"
    } else {
        error.as_str()
    };
    frame.render_widget(
        Paragraph::new(info)
            .style(Style::default().fg(if error.is_empty() {
                theme.muted
            } else {
                theme.error
            }))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y + 13, inner.width, 2),
    );
    let hint = if dialog.editing.is_some() {
        "Enter accepts value · Esc cancels edit · Ctrl+U clears"
    } else {
        "Enter prints · Tab / ↑↓ choose · ←→ change · Space edits · Esc cancels"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(hint).style(Style::default().fg(theme.accent)),
            Line::from("Unsaved edits are included. Pages: all / current / selected / 1,3-5")
                .style(Style::default().fg(theme.muted)),
        ])
        .wrap(Wrap { trim: true }),
        Rect::new(
            inner.x,
            inner.y + 16,
            inner.width,
            inner.height.saturating_sub(16),
        ),
    );
}

pub fn confirm(state: &mut State, label: &str) -> Result<bool> {
    if !io::IsTerminal::is_terminal(&io::stdin()) {
        return Ok(false);
    }
    let mut runtime = match state.ui.take() {
        Some(runtime) => runtime,
        None => Runtime::new()?,
    };
    crate::clear_terminal_images("kitty")?;
    let result = (|| loop {
        runtime.terminal.draw(|frame| {
            render(frame, state);
            let area = centered(frame.area(), 76, 7);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(format!("{label}\n\ny  Yes     n / Esc  Cancel"))
                    .wrap(Wrap { trim: true })
                    .block(panel(" Confirm ", &state.config.theme))
                    .style(
                        Style::default()
                            .bg(state.config.theme.background)
                            .fg(state.config.theme.foreground),
                    ),
                area,
            );
        })?;
        match actions::normal_input(crate::read_input()?) {
            Input::Char('y' | 'Y') => return Ok(true),
            Input::Char('n' | 'N' | 'q') | Input::Esc | Input::CtrlC | Input::Enter => {
                return Ok(false)
            }
            _ => {}
        }
    })();
    runtime.invalidate();
    state.ui = Some(runtime);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn widgets_render_at_small_and_large_sizes() -> Result<()> {
        for (width, height) in [(35, 10), (110, 40), (8, 4)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            let mut state = State::new(
                crate::Args {
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
            state.help = Some(actions::Help::default());
            terminal.draw(|frame| render(frame, &state))?;
            assert!(terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol().contains('A')));
        }
        Ok(())
    }
}
