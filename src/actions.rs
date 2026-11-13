use crate::Input;

pub struct Action {
    pub group: &'static str,
    pub name: &'static str,
    pub keys: &'static str,
    pub command: &'static str,
    pub detail: &'static str,
}

macro_rules! action {
    ($group:literal, $name:literal, $keys:literal, $command:literal, $detail:literal) => {
        Action {
            group: $group,
            name: $name,
            keys: $keys,
            command: $command,
            detail: $detail,
        }
    };
}

pub const ACTIONS: &[Action] = &[
    action!(
        "Navigate",
        "Next page",
        "n / l / → / PgDn",
        "next",
        "Move forward one page"
    ),
    action!(
        "Navigate",
        "Previous page",
        "p / h / ← / PgUp",
        "prev",
        "Move backward one page"
    ),
    action!(
        "Navigate",
        "First page",
        "g / Home",
        "first",
        "Jump to the beginning"
    ),
    action!(
        "Navigate",
        "Last page",
        "G / End",
        "last",
        "Jump to the end"
    ),
    action!(
        "Navigate",
        "Go to page…",
        "o",
        "page ",
        "Enter a page number; :42 also works"
    ),
    action!(
        "Select",
        "Mark / unmark current page",
        "m / Space",
        "mark",
        "Edit or print multiple marked pages together"
    ),
    action!(
        "Select",
        "Select page range…",
        "v",
        "select ",
        "Use 1,3-5 or all"
    ),
    action!(
        "Select",
        "Select all pages",
        "a",
        "select all",
        "Mark the entire document"
    ),
    action!(
        "Select",
        "Unselect page range…",
        "V",
        "unselect ",
        "Remove a range from the marked pages"
    ),
    action!(
        "Select",
        "Clear page selection",
        "A",
        "clear",
        "Return to editing just the visible page"
    ),
    action!(
        "Edit",
        "Rotate clockwise",
        "r",
        "rotate",
        "Rotate selected/current pages right 90°"
    ),
    action!(
        "Edit",
        "Rotate counterclockwise",
        "R",
        "rotate-left",
        "Rotate selected/current pages left 90°"
    ),
    action!(
        "Edit",
        "Mirror page",
        "M",
        "mirror",
        "Flip selected/current pages horizontally"
    ),
    action!(
        "Edit",
        "Delete pages",
        "d / Delete",
        "delete",
        "Remove selected/current pages; undo is available"
    ),
    action!(
        "Edit",
        "Crop pages",
        "c",
        "crop",
        "Interactive crop; arrows/hjkl adjust, Enter applies, Esc cancels"
    ),
    action!(
        "Edit",
        "Move pages before…",
        "J",
        "move ",
        "Enter the original target page number"
    ),
    action!(
        "Edit",
        "Move pages backward",
        "<",
        "back",
        "Move selected/current pages one position earlier"
    ),
    action!(
        "Edit",
        "Move pages forward",
        ">",
        "forward",
        "Move selected/current pages one position later"
    ),
    action!(
        "Compress",
        "Compress · normal",
        "z n",
        "compress normal",
        "Balanced file size and detail; rasterizes affected pages"
    ),
    action!(
        "Compress",
        "Compress · light",
        "z l",
        "compress light",
        "Keep more detail; rasterizes affected pages"
    ),
    action!(
        "Compress",
        "Compress · strong",
        "z s",
        "compress strong",
        "Smallest file; rasterizes affected pages"
    ),
    action!(
        "Add",
        "Append images / PDFs…",
        "i",
        "add ",
        "Type paths; quotes support spaces"
    ),
    action!(
        "Add",
        "Browse files to append",
        "O",
        "ls",
        "Space marks files; Enter opens directories or appends files"
    ),
    action!(
        "Add",
        "Scan new pages",
        "S",
        "scan",
        "Scan via SANE; Enter scans next sheet or retries; Esc finishes; :scan 300 sets DPI"
    ),
    action!(
        "History",
        "Undo",
        "u / Ctrl+Z",
        "undo",
        "Restore the preceding edit"
    ),
    action!(
        "History",
        "Redo",
        "U / Ctrl+R",
        "redo",
        "Restore the newest child edit"
    ),
    action!(
        "History",
        "Browse history tree",
        "t",
        "tree",
        "Navigate branches with arrows/jk; Enter restores a state"
    ),
    action!(
        "History",
        "Go to history node…",
        "H",
        "goto ",
        "Enter a node number from the history tree"
    ),
    action!(
        "File",
        "Save",
        "s",
        "w",
        "Save to the current target; input is preserved by default"
    ),
    action!("File", "Save as…", "w", "save ", "Use a path or NAME DIR"),
    action!(
        "File",
        "Rename / move output…",
        "N",
        "rename ",
        "Rename the saved output or change the output target"
    ),
    action!(
        "File",
        "Save and quit",
        "Z Z",
        "wq",
        "Save successfully before closing"
    ),
    action!(
        "Print",
        "Print…",
        "Ctrl+P / P",
        "print",
        "Printer, pages, copies, monochrome/colour, orientation, paper and quality; Enter prints"
    ),
    action!(
        "Print",
        "Quick print",
        "Alt+P",
        "print!",
        "Submit immediately using defaults and marked pages (or all)"
    ),
    action!(
        "Config",
        "Reload theme / defaults",
        "F5",
        "reload",
        "Read ~/.config/pdfview/config.toml without restarting"
    ),
    action!(
        "Config",
        "Show config location",
        "C",
        "config",
        "Display the active configuration path"
    ),
    action!(
        "Help",
        "Find an action",
        "F1 / ?",
        "help",
        "Search names, shortcuts and Vim-style commands"
    ),
    action!(
        "File",
        "Quit",
        "q / Esc",
        "q",
        "Close; unsaved changes require :q! or :wq"
    ),
    action!(
        "File",
        "Quit without saving",
        "Z Q",
        "q!",
        "Discard unsaved edits"
    ),
];

pub fn matching(query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut matches: Vec<(i32, usize)> = ACTIONS
        .iter()
        .enumerate()
        .filter_map(|(index, action)| {
            let haystack = format!(
                "{} {} {} {} {}",
                action.group, action.name, action.keys, action.command, action.detail
            )
            .to_lowercase();
            if !words.iter().all(|word| haystack.contains(word)) {
                return None;
            }
            let query = words.join(" ");
            let name = action.name.to_lowercase();
            let command = action.command.to_lowercase();
            let score = if query.is_empty() {
                0
            } else if name.starts_with(&query) {
                900
            } else if command.trim() == query {
                850
            } else if words.iter().all(|word| name.contains(word)) {
                700
            } else if words.iter().all(|word| command.contains(word)) {
                600
            } else if action.keys.to_lowercase().contains(&query) {
                500
            } else {
                100
            };
            Some((score, index))
        })
        .collect();
    matches.sort_by_key(|(score, index)| (std::cmp::Reverse(*score), *index));
    matches.into_iter().map(|(_, index)| index).collect()
}

pub fn shortcut_char(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    let extra = match c {
        'і' => Some('s'),
        'ї' => Some(']'),
        'є' => Some('\''),
        'ґ' => Some('\\'),
        _ => None,
    };
    extra.unwrap_or_else(|| {
        "йцукенгшщзхъфывапролджэячсмитьбюё"
            .chars()
            .position(|key| key == c)
            .and_then(|index| "qwertyuiop[]asdfghjkl;'zxcvbnm,.`".chars().nth(index))
            .unwrap_or(c)
    })
}

pub fn normal_input(input: Input) -> Input {
    if let Input::Char(c) = input {
        let mapped = shortcut_char(c);
        Input::Char(if c.is_uppercase() {
            match mapped {
                ',' => '<',
                '.' => '>',
                ';' => ':',
                '\'' => '"',
                '[' => '{',
                ']' => '}',
                '`' => '~',
                '\\' => '|',
                _ => mapped.to_ascii_uppercase(),
            }
        } else {
            mapped
        })
    } else {
        input
    }
}

pub fn binding(input: Input) -> Option<&'static str> {
    match input {
        Input::Char('n' | 'l') | Input::Right | Input::PageDown | Input::WheelDown => Some("next"),
        Input::Char('p' | 'h') | Input::Left | Input::PageUp | Input::WheelUp => Some("prev"),
        Input::Char('g') | Input::Home => Some("first"),
        Input::Char('G') | Input::End => Some("last"),
        Input::Char('o') => Some("page "),
        Input::Char('m' | ' ') => Some("mark"),
        Input::Char('v') => Some("select "),
        Input::Char('V') => Some("unselect "),
        Input::Char('a') => Some("select all"),
        Input::Char('A') => Some("clear"),
        Input::Char('r') => Some("rotate"),
        Input::Char('R') => Some("rotate-left"),
        Input::Char('M') => Some("mirror"),
        Input::Char('d') | Input::Delete => Some("delete"),
        Input::Char('c') => Some("crop"),
        Input::Char('J') => Some("move "),
        Input::Char('<') => Some("back"),
        Input::Char('>') => Some("forward"),
        Input::Char('i') => Some("add "),
        Input::Char('O') => Some("ls"),
        Input::Char('S') => Some("scan"),
        Input::Char('u') | Input::CtrlZ => Some("undo"),
        Input::Char('U') | Input::Ctrl('r') => Some("redo"),
        Input::Char('t') => Some("tree"),
        Input::Char('H') => Some("goto "),
        Input::Char('s') => Some("w"),
        Input::Char('w') => Some("save "),
        Input::Char('N') => Some("rename "),
        Input::Ctrl('p') | Input::Char('P') => Some("print"),
        Input::Alt('p') => Some("print!"),
        Input::Function(5) => Some("reload"),
        Input::Char('C') => Some("config"),
        Input::Function(1) | Input::Char('?') => Some("help"),
        Input::Char('q' | 'x') | Input::Esc => Some("q"),
        _ => None,
    }
}

#[derive(Clone, Default, Debug)]
pub struct LineInput {
    pub text: String,
    pub cursor: usize,
}

impl LineInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            cursor: text.chars().count(),
            text,
        }
    }

    pub fn edit(&mut self, input: Input) -> bool {
        let mut chars: Vec<char> = self.text.chars().collect();
        self.cursor = self.cursor.min(chars.len());
        match input {
            Input::Char(c) => {
                chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            Input::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                chars.remove(self.cursor);
            }
            Input::Delete if self.cursor < chars.len() => {
                chars.remove(self.cursor);
            }
            Input::Left => self.cursor = self.cursor.saturating_sub(1),
            Input::Right => self.cursor = (self.cursor + 1).min(chars.len()),
            Input::Home | Input::Ctrl('a') => self.cursor = 0,
            Input::End | Input::Ctrl('e') => self.cursor = chars.len(),
            Input::Ctrl('u') => {
                chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Input::Ctrl('k') => {
                chars.truncate(self.cursor);
            }
            _ => return false,
        }
        self.text = chars.into_iter().collect();
        true
    }
}

#[derive(Default)]
pub struct Help {
    pub query: LineInput,
    pub selected: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_finds_commands_shortcuts_and_features() {
        for query in [
            "print",
            "Ctrl+P",
            "monochrome",
            "crop",
            "save as",
            "page range",
        ] {
            assert!(!matching(query).is_empty(), "{query}");
        }
        assert!(matching("not-an-action").is_empty());
        assert_eq!(ACTIONS[matching("print")[0]].command, "print");
    }

    #[test]
    fn editor_handles_unicode_and_cursor() {
        let mut line = LineInput::new("aїb");
        line.edit(Input::Left);
        line.edit(Input::Backspace);
        line.edit(Input::Char('ф'));
        assert_eq!(line.text, "aфb");
        line.edit(Input::Home);
        line.edit(Input::Delete);
        assert_eq!(line.text, "фb");
    }
}
