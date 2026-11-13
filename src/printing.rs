use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::Path,
    process::{Command, Output, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::{actions::LineInput, Input};

fn read_output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("CUPS query timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

macro_rules! choice {
    ($name:ident { $first:ident => $label:literal, $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum $name { #[default] $first, $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[Self::$first, $(Self::$variant),+];
            pub fn label(self) -> &'static str {
                match self { Self::$first => $label, $(Self::$variant => $text),+ }
            }
            fn cycle(self, delta: i32) -> Self {
                let index = Self::ALL.iter().position(|value| *value == self).unwrap_or(0);
                Self::ALL[(index as i32 + delta).rem_euclid(Self::ALL.len() as i32) as usize]
            }
        }
    };
}

choice!(ColorMode { Auto => "Printer default", Monochrome => "Black & white", Color => "Colour" });
choice!(Orientation { Auto => "Document / printer default", Portrait => "Portrait", Landscape => "Landscape" });
choice!(Sides { Auto => "Printer default", OneSided => "One-sided", LongEdge => "Duplex · long edge", ShortEdge => "Duplex · short edge" });
choice!(PaperType { Auto => "Printer default", Plain => "Plain paper", Photo => "Glossy photo paper" });
choice!(Quality { Auto => "Printer default", Draft => "Draft / fast", Normal => "Normal", High => "High / best" });
choice!(Scaling { Auto => "Printer default", Fit => "Fit to printable area", Actual => "Actual size (100%)" });

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrintOptions {
    pub printer: Option<String>,
    pub pages: String,
    pub copies: u16,
    pub color: ColorMode,
    pub orientation: Orientation,
    pub sides: Sides,
    pub paper: Option<String>,
    pub paper_type: PaperType,
    pub quality: Quality,
    pub scaling: Scaling,
}

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            printer: None,
            pages: "all".into(),
            copies: 1,
            color: ColorMode::Auto,
            orientation: Orientation::Auto,
            sides: Sides::Auto,
            paper: None,
            paper_type: PaperType::Auto,
            quality: Quality::Auto,
            scaling: Scaling::Auto,
        }
    }
}

impl PrintOptions {
    pub fn validate(&self) -> Result<()> {
        if !(1..=999).contains(&self.copies) {
            bail!("copies must be between 1 and 999");
        }
        if self.pages.trim().is_empty() {
            bail!("pages must be all, current, selected, or a range like 1,3-5");
        }
        Ok(())
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "printer" => {
                self.printer = (!value.is_empty() && value != "default").then(|| value.into())
            }
            "pages" => self.pages = value.into(),
            "copies" => self.copies = value.parse().context("copies must be a positive number")?,
            "color" | "colour" => {
                self.color = match value {
                    "auto" => ColorMode::Auto,
                    "bw" | "mono" | "monochrome" | "gray" => ColorMode::Monochrome,
                    "color" | "colour" => ColorMode::Color,
                    _ => bail!("color: auto, monochrome, or color"),
                }
            }
            "orientation" => {
                self.orientation = match value {
                    "auto" => Orientation::Auto,
                    "portrait" => Orientation::Portrait,
                    "landscape" => Orientation::Landscape,
                    _ => bail!("orientation: auto, portrait, or landscape"),
                }
            }
            "sides" => {
                self.sides = match value {
                    "auto" => Sides::Auto,
                    "one-sided" | "simplex" => Sides::OneSided,
                    "long-edge" | "long" => Sides::LongEdge,
                    "short-edge" | "short" => Sides::ShortEdge,
                    _ => bail!("sides: auto, one-sided, long-edge, or short-edge"),
                }
            }
            "paper" => self.paper = (!value.is_empty() && value != "default").then(|| value.into()),
            "type" | "paper-type" => {
                self.paper_type = match value {
                    "auto" => PaperType::Auto,
                    "plain" => PaperType::Plain,
                    "photo" => PaperType::Photo,
                    _ => bail!("type: auto, plain, or photo"),
                }
            }
            "quality" => {
                self.quality = match value {
                    "auto" => Quality::Auto,
                    "draft" => Quality::Draft,
                    "normal" => Quality::Normal,
                    "high" | "best" => Quality::High,
                    _ => bail!("quality: auto, draft, normal, or high"),
                }
            }
            "scale" | "scaling" => {
                self.scaling = match value {
                    "auto" => Scaling::Auto,
                    "fit" => Scaling::Fit,
                    "actual" => Scaling::Actual,
                    _ => bail!("scale: auto, fit, or actual"),
                }
            }
            _ => bail!("unknown print option: {key}"),
        }
        self.validate()
    }
}

pub fn options_from_args(
    defaults: &PrintOptions,
    args: &[String],
    has_selection: bool,
) -> Result<PrintOptions> {
    let mut options = defaults.clone();
    if has_selection && options.pages == "all" {
        options.pages = "selected".into();
    }
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("print options use key=value; see F1 → Print"))?;
        options.set(key, value)?;
    }
    Ok(options)
}

#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub known: bool,
    pub options: BTreeMap<String, Vec<String>>,
}

impl Capabilities {
    pub fn parse(text: &str) -> Self {
        let mut options = BTreeMap::new();
        for line in text.lines() {
            if let Some((label, values)) = line.split_once(':') {
                let key = label.split('/').next().unwrap_or(label);
                options.insert(
                    key.into(),
                    values
                        .split_whitespace()
                        .map(|v| v.trim_start_matches('*').into())
                        .collect(),
                );
            }
        }
        Self {
            known: !options.is_empty(),
            options,
        }
    }

    pub fn load(printer: Option<&str>) -> Self {
        let mut command = Command::new("lpoptions");
        if let Some(printer) = printer {
            command.args(["-p", printer]);
        }
        command.arg("-l").env("LC_ALL", "C");
        read_output(&mut command, Duration::from_secs(2))
            .ok()
            .filter(|output| output.status.success())
            .map(|output| Self::parse(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default()
    }

    fn value(&self, key: &str, preferred: &[&str]) -> Option<&str> {
        let values = self.options.get(key)?;
        preferred.iter().find_map(|preferred| {
            values
                .iter()
                .find(|value| value.eq_ignore_ascii_case(preferred))
                .map(String::as_str)
        })
    }

    pub fn duplex(&self) -> bool {
        !self.known
            || self
                .value(
                    "Duplex",
                    &["DuplexNoTumble", "DuplexTumble", "LongEdge", "ShortEdge"],
                )
                .is_some()
            || self
                .value("sides", &["two-sided-long-edge", "two-sided-short-edge"])
                .is_some()
    }
}

#[derive(Default)]
pub struct Printers {
    pub names: Vec<String>,
    pub default: Option<String>,
    pub error: String,
}

pub fn discover() -> Printers {
    let mut printers = Printers::default();
    match read_output(
        Command::new("lpstat").arg("-p").env("LC_ALL", "C"),
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            printers.names = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("printer ")?
                        .split_whitespace()
                        .next()
                        .map(str::to_string)
                })
                .collect();
        }
        Ok(output) => printers.error = String::from_utf8_lossy(&output.stderr).trim().into(),
        Err(error) => printers.error = format!("Cannot read CUPS printers: {error}"),
    }
    if !printers.error.is_empty() {
        return printers;
    }
    if let Ok(output) = read_output(
        Command::new("lpstat").arg("-d").env("LC_ALL", "C"),
        Duration::from_secs(2),
    ) {
        if output.status.success() {
            printers.default = String::from_utf8_lossy(&output.stdout)
                .split_once(':')
                .map(|(_, value)| value.trim().into());
        }
    }
    printers
}

pub fn printer_info(printer: Option<&str>) -> (Printers, Capabilities) {
    let printers = discover();
    let target = printer.or(printers.default.as_deref());
    let caps = if target.is_some() {
        Capabilities::load(target)
    } else {
        Capabilities::default()
    };
    (printers, caps)
}

fn probe(printer: Option<String>) -> Receiver<(Printers, Capabilities)> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(printer_info(printer.as_deref()));
    });
    receiver
}

pub fn lp_arguments(
    options: &PrintOptions,
    caps: &Capabilities,
    title: &str,
    pdf: &Path,
) -> Result<Vec<OsString>> {
    options.validate()?;
    let mut args: Vec<OsString> = vec![
        "-t".into(),
        title.into(),
        "-n".into(),
        options.copies.to_string().into(),
    ];
    if let Some(printer) = &options.printer {
        args.extend(["-d".into(), printer.into()]);
    }
    let mut option = |value: String| args.extend([OsString::from("-o"), value.into()]);
    if options.copies > 1 {
        option("collate=true".into());
    }
    if options.color != ColorMode::Auto {
        let (ipp, values): (&str, &[&str]) = if options.color == ColorMode::Monochrome {
            (
                "monochrome",
                &["KGray", "Gray", "Grayscale", "Black", "Mono", "CMYGray"],
            )
        } else {
            ("color", &["RGB", "sRGB", "Color", "CMYK"])
        };
        option(format!("print-color-mode={ipp}"));
        if let Some(value) = caps.value("ColorModel", values) {
            option(format!("ColorModel={value}"));
        }
    }
    match options.orientation {
        Orientation::Auto => {}
        Orientation::Portrait => option("orientation-requested=3".into()),
        Orientation::Landscape => option("orientation-requested=4".into()),
    }
    match options.sides {
        Sides::Auto => {}
        Sides::OneSided => option("sides=one-sided".into()),
        Sides::LongEdge | Sides::ShortEdge => {
            if !caps.duplex() {
                bail!("this printer does not report automatic duplex support");
            }
            option(format!(
                "sides={}",
                if options.sides == Sides::LongEdge {
                    "two-sided-long-edge"
                } else {
                    "two-sided-short-edge"
                }
            ));
        }
    }
    if let Some(paper) = &options.paper {
        if let Some(sizes) = caps.options.get("PageSize") {
            if !sizes.iter().any(|size| size.eq_ignore_ascii_case(paper)) {
                bail!("printer does not report paper size {paper}; use printer default or a supported size");
            }
        }
        option(format!("media={paper}"));
    }
    if options.paper_type != PaperType::Auto {
        let values: &[&str] = if options.paper_type == PaperType::Plain {
            &["Plain", "PlainPaper"]
        } else {
            &["Glossy", "Photo", "PhotographicGlossy"]
        };
        if let Some(value) = caps.value("MediaType", values) {
            option(format!("MediaType={value}"));
        } else {
            option(format!(
                "media-type={}",
                if options.paper_type == PaperType::Plain {
                    "stationery"
                } else {
                    "photographic-glossy"
                }
            ));
        }
    }
    if options.quality != Quality::Auto {
        let (ipp, values): (&str, &[&str]) = match options.quality {
            Quality::Draft => ("3", &["FastDraft", "Draft"]),
            Quality::Normal => ("4", &["Normal"]),
            Quality::High => ("5", &["Best", "High", "Photo"]),
            Quality::Auto => unreachable!(),
        };
        if let Some(value) = caps.value("OutputMode", values) {
            option(format!("OutputMode={value}"));
        } else {
            option(format!("print-quality={ipp}"));
        }
    }
    match options.scaling {
        Scaling::Auto => {}
        Scaling::Fit => {
            option("print-scaling=fit".into());
            option("fit-to-page=true".into());
        }
        Scaling::Actual => {
            option("print-scaling=none".into());
            option("fit-to-page=false".into());
        }
    }
    args.push(pdf.as_os_str().to_owned());
    Ok(args)
}

pub fn submit(
    options: &PrintOptions,
    caps: &Capabilities,
    title: &str,
    pdf: &Path,
) -> Result<String> {
    run_lp(
        Command::new("lp")
            .args(lp_arguments(options, caps, title, pdf)?)
            .env("LC_ALL", "C"),
    )
}

fn run_lp(command: &mut Command) -> Result<String> {
    let output = command
        .output()
        .context("start CUPS lp; install cups if unavailable")?;
    if !output.status.success() {
        bail!("CUPS: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

pub const FIELD_NAMES: &[&str] = &[
    "Printer [p]",
    "Pages [r]",
    "Copies [n]",
    "Colour [c]",
    "Orientation [o]",
    "Sides [d]",
    "Paper [m]",
    "Paper type [t]",
    "Quality [q]",
    "Scaling [f]",
];
pub const SUBMIT: usize = FIELD_NAMES.len();

pub struct Dialog {
    pub options: PrintOptions,
    pub printers: Printers,
    pub caps: Capabilities,
    pub focus: usize,
    pub editing: Option<LineInput>,
    pub error: String,
    pub has_selection: bool,
    pub pending_submit: bool,
    probe: Option<Receiver<(Printers, Capabilities)>>,
}

pub enum DialogEvent {
    Continue,
    Cancel,
    Submit,
}

impl Dialog {
    pub fn open(options: PrintOptions, has_selection: bool) -> Self {
        let probe = Some(probe(options.printer.clone()));
        Self {
            options,
            printers: Printers::default(),
            caps: Capabilities::default(),
            focus: SUBMIT,
            editing: None,
            error: String::new(),
            has_selection,
            pending_submit: false,
            probe,
        }
    }

    pub fn loading(&self) -> bool {
        self.probe.is_some()
    }

    pub fn poll(&mut self) -> bool {
        let Some(receiver) = &self.probe else {
            return false;
        };
        match receiver.try_recv() {
            Ok((printers, caps)) => {
                if !printers.error.is_empty() {
                    self.pending_submit = false;
                    self.error = printers.error.clone();
                }
                self.printers = printers;
                self.caps = caps;
                self.probe = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.probe = None;
                self.pending_submit = false;
                self.error = "Could not load printer settings".into();
                true
            }
        }
    }

    pub fn values(&self) -> Vec<String> {
        vec![
            self.options.printer.clone().unwrap_or_else(|| {
                format!(
                    "System default{}",
                    self.printers
                        .default
                        .as_ref()
                        .map(|p| format!(" · {p}"))
                        .unwrap_or_default()
                )
            }),
            self.options.pages.clone(),
            self.options.copies.to_string(),
            self.options.color.label().into(),
            self.options.orientation.label().into(),
            if self.caps.duplex() {
                self.options.sides.label().into()
            } else {
                "One-sided · no automatic duplex".into()
            },
            self.options
                .paper
                .clone()
                .unwrap_or_else(|| "Printer default".into()),
            self.options.paper_type.label().into(),
            self.options.quality.label().into(),
            self.options.scaling.label().into(),
        ]
    }

    fn commit_edit(&mut self, text: &str) -> Result<()> {
        match self.focus {
            0 => self.options.set("printer", text)?,
            1 => self.options.set("pages", text)?,
            2 => self.options.set("copies", text)?,
            6 => self.options.set("paper", text)?,
            _ => {}
        }
        if self.focus == 0 {
            self.reload_caps();
        }
        Ok(())
    }

    fn reload_caps(&mut self) {
        self.probe = Some(probe(self.options.printer.clone()));
    }

    fn cycle(&mut self, delta: i32) {
        match self.focus {
            0 => {
                let mut printers: Vec<Option<String>> = vec![None];
                printers.extend(self.printers.names.iter().cloned().map(Some));
                let index = printers
                    .iter()
                    .position(|p| *p == self.options.printer)
                    .unwrap_or(0);
                self.options.printer = printers
                    [(index as i32 + delta).rem_euclid(printers.len() as i32) as usize]
                    .clone();
                self.reload_caps();
            }
            1 => {
                let choices = if self.has_selection {
                    vec!["all", "current", "selected"]
                } else {
                    vec!["all", "current"]
                };
                let index = choices
                    .iter()
                    .position(|p| *p == self.options.pages)
                    .unwrap_or(0);
                self.options.pages = choices
                    [(index as i32 + delta).rem_euclid(choices.len() as i32) as usize]
                    .into();
            }
            2 => {
                self.options.copies = (i32::from(self.options.copies) + delta).clamp(1, 999) as u16
            }
            3 => self.options.color = self.options.color.cycle(delta),
            4 => self.options.orientation = self.options.orientation.cycle(delta),
            5 => {
                if self.caps.duplex() {
                    self.options.sides = self.options.sides.cycle(delta);
                } else {
                    self.options.sides = Sides::OneSided;
                }
            }
            6 => {
                let choices = [None, Some("A4"), Some("A5"), Some("Letter"), Some("Legal")];
                let index = choices
                    .iter()
                    .position(|p| *p == self.options.paper.as_deref())
                    .unwrap_or(0);
                self.options.paper = choices
                    [(index as i32 + delta).rem_euclid(choices.len() as i32) as usize]
                    .map(str::to_string);
            }
            7 => self.options.paper_type = self.options.paper_type.cycle(delta),
            8 => self.options.quality = self.options.quality.cycle(delta),
            9 => self.options.scaling = self.options.scaling.cycle(delta),
            _ => {}
        }
    }

    pub fn input(&mut self, input: Input) -> DialogEvent {
        if matches!(input, Input::Unknown) {
            return DialogEvent::Continue;
        }
        self.pending_submit = false;
        if let Some(mut editing) = self.editing.take() {
            match input {
                Input::Esc | Input::CtrlC => {}
                Input::Enter => {
                    if let Err(error) = self.commit_edit(editing.text.trim()) {
                        self.error = error.to_string();
                        self.editing = Some(editing);
                    }
                }
                _ => {
                    editing.edit(input);
                    self.editing = Some(editing);
                }
            }
            return DialogEvent::Continue;
        }
        match input {
            Input::Esc | Input::CtrlC => return DialogEvent::Cancel,
            Input::Enter => {
                if self.loading() {
                    self.pending_submit = true;
                    return DialogEvent::Continue;
                }
                return DialogEvent::Submit;
            }
            Input::Tab | Input::Down | Input::Char('j') => {
                self.focus = (self.focus + 1) % (SUBMIT + 1)
            }
            Input::BackTab | Input::Up | Input::Char('k') => {
                self.focus = (self.focus + SUBMIT) % (SUBMIT + 1)
            }
            Input::Right | Input::Char('l') => self.cycle(1),
            Input::Left | Input::Char('h') => self.cycle(-1),
            Input::Char(' ' | 'e') => {
                if [0, 1, 2, 6].contains(&self.focus) {
                    let value = match self.focus {
                        0 => self.options.printer.clone().unwrap_or_default(),
                        6 => self.options.paper.clone().unwrap_or_default(),
                        _ => self.values()[self.focus].clone(),
                    };
                    self.editing = Some(LineInput::new(value));
                } else {
                    self.cycle(1);
                }
            }
            Input::Char(c) => {
                if let Some(index) = "prncodmtqf".find(c) {
                    self.focus = index;
                }
            }
            _ => {}
        }
        self.error.clear();
        DialogEvent::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn explicit_all_overrides_automatic_marked_page_default() -> Result<()> {
        let defaults = PrintOptions::default();
        assert_eq!(options_from_args(&defaults, &[], true)?.pages, "selected");
        let options = options_from_args(&defaults, &["pages=all".into()], true)?;
        assert_eq!(options.pages, "all");
        Ok(())
    }

    #[test]
    fn dialog_edits_ranges_and_colour_without_submitting() {
        let mut dialog = Dialog {
            options: PrintOptions::default(),
            printers: Printers::default(),
            caps: Capabilities::parse("ColorModel/Colour: RGB KGray"),
            focus: SUBMIT,
            editing: None,
            error: String::new(),
            has_selection: true,
            pending_submit: false,
            probe: None,
        };
        dialog.input(Input::Char('r'));
        dialog.input(Input::Char(' '));
        dialog.input(Input::Ctrl('u'));
        for c in "1,3-5".chars() {
            dialog.input(Input::Char(c));
        }
        assert!(matches!(dialog.input(Input::Enter), DialogEvent::Continue));
        assert_eq!(dialog.options.pages, "1,3-5");
        dialog.input(Input::Char('c'));
        dialog.input(Input::Right);
        assert_eq!(dialog.options.color, ColorMode::Monochrome);
        assert!(matches!(dialog.input(Input::Enter), DialogEvent::Submit));
        assert!(matches!(dialog.input(Input::Esc), DialogEvent::Cancel));
    }

    #[test]
    fn printer_loading_keeps_enter_pending_and_allows_cancel() {
        let (sender, receiver) = mpsc::channel();
        let mut dialog = Dialog {
            options: PrintOptions::default(),
            printers: Printers::default(),
            caps: Capabilities::default(),
            focus: SUBMIT,
            editing: None,
            error: String::new(),
            has_selection: false,
            pending_submit: false,
            probe: Some(receiver),
        };
        assert!(matches!(dialog.input(Input::Enter), DialogEvent::Continue));
        assert!(dialog.pending_submit);
        assert!(!dialog.poll());
        dialog.input(Input::Unknown);
        assert!(dialog.pending_submit);
        sender
            .send((Printers::default(), Capabilities::default()))
            .unwrap();
        assert!(dialog.poll());
        assert!(!dialog.loading());
        assert!(dialog.pending_submit);
        assert!(matches!(dialog.input(Input::Esc), DialogEvent::Cancel));
        assert!(!dialog.pending_submit);

        let (sender, receiver) = mpsc::channel();
        dialog.probe = Some(receiver);
        dialog.input(Input::Enter);
        drop(sender);
        assert!(dialog.poll());
        assert!(!dialog.pending_submit);
        assert!(!dialog.error.is_empty());
    }

    #[test]
    fn cups_metadata_timeout_stops_the_child() {
        let started = Instant::now();
        let result = read_output(Command::new("sleep").arg("10"), Duration::from_millis(30));
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cups_success_and_failure_are_reported_without_real_printing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let fake = temp.path().join("lp");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf 'request id is mock-42 (1 file(s))\\n'\n",
        )?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))?;
        assert_eq!(
            run_lp(&mut Command::new(&fake))?,
            "request id is mock-42 (1 file(s))"
        );
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf 'printer unavailable\\n' >&2\nexit 1\n",
        )?;
        assert!(run_lp(&mut Command::new(&fake))
            .unwrap_err()
            .to_string()
            .contains("printer unavailable"));
        Ok(())
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn defaults_preserve_printer_settings_and_filename() -> Result<()> {
        let args = strings(lp_arguments(
            &PrintOptions::default(),
            &Capabilities::default(),
            "My scan",
            Path::new("/tmp/file with spaces.pdf"),
        )?);
        assert_eq!(
            args,
            ["-t", "My scan", "-n", "1", "/tmp/file with spaces.pdf"]
        );
        Ok(())
    }

    #[test]
    fn hp_black_ink_and_quality_use_real_driver_values() -> Result<()> {
        let caps = Capabilities::parse("ColorModel/Output: *RGB CMYGray KGray\nOutputMode/Quality: Normal FastDraft *Best\nMediaType/Type: *Plain Glossy");
        let options = PrintOptions {
            color: ColorMode::Monochrome,
            quality: Quality::Draft,
            copies: 2,
            paper_type: PaperType::Photo,
            orientation: Orientation::Landscape,
            ..PrintOptions::default()
        };
        let args = strings(lp_arguments(
            &options,
            &caps,
            "scan",
            Path::new("/tmp/file.pdf"),
        )?);
        for expected in [
            "ColorModel=KGray",
            "OutputMode=FastDraft",
            "MediaType=Glossy",
            "orientation-requested=4",
            "collate=true",
        ] {
            assert!(args.contains(&expected.into()), "{expected}");
        }
        let duplex = PrintOptions {
            sides: Sides::LongEdge,
            ..options
        };
        assert!(lp_arguments(&duplex, &caps, "scan", Path::new("file.pdf")).is_err());
        Ok(())
    }

    #[test]
    fn ipp_printer_options_and_validation() -> Result<()> {
        let mut options = PrintOptions::default();
        for (key, value) in [
            ("color", "color"),
            ("sides", "long"),
            ("quality", "high"),
            ("scale", "fit"),
            ("pages", "1,3-5"),
        ] {
            options.set(key, value)?;
        }
        let args = strings(lp_arguments(
            &options,
            &Capabilities::default(),
            "scan",
            Path::new("file.pdf"),
        )?);
        for expected in [
            "print-color-mode=color",
            "sides=two-sided-long-edge",
            "print-quality=5",
            "print-scaling=fit",
        ] {
            assert!(args.contains(&expected.into()));
        }
        assert!(options.set("copies", "0").is_err());
        assert!(options.set("unknown", "value").is_err());
        Ok(())
    }
}
