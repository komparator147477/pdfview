use std::{
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tempfile::TempDir;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub device: Option<String>,
    pub dpi: u32,
    pub mode: String,
    pub source: Option<String>,
    pub timeout_seconds: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device: None,
            dpi: 300,
            mode: "Color".into(),
            source: None,
            timeout_seconds: 300,
        }
    }
}

impl Options {
    pub fn validate(&self) -> Result<()> {
        if !(1..=9600).contains(&self.dpi) {
            bail!("scan dpi must be between 1 and 9600");
        }
        if self.mode.trim().is_empty() {
            bail!("scan mode must not be empty");
        }
        if !(1..=3600).contains(&self.timeout_seconds) {
            bail!("scan timeout_seconds must be between 1 and 3600");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub message: String,
    pub percent: Option<f64>,
    pub device: Option<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            message: "Finding scanner…".into(),
            percent: None,
            device: None,
        }
    }
}

pub enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Default)]
struct Shared {
    progress: Progress,
    outcome: Option<Outcome>,
}

pub struct Job {
    pub output: PathBuf,
    shared: Arc<Mutex<Shared>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _temp: TempDir,
}

pub struct Session {
    pub options: Options,
    pub job: Option<Job>,
    pub progress: Progress,
    pub error: Option<String>,
    pub cancelling: bool,
    pub pages_added: usize,
    pub ready: bool,
}

pub const DEVICE_CACHE_DURATION: Duration = Duration::from_secs(120);

pub struct CachedDevice {
    device: String,
    last_used: Instant,
}

impl CachedDevice {
    pub fn remember(device: String) -> Self {
        Self {
            device,
            last_used: Instant::now(),
        }
    }

    pub fn device(&self) -> Option<&str> {
        (self.last_used.elapsed() < DEVICE_CACHE_DURATION).then_some(self.device.as_str())
    }
}

impl Session {
    pub fn start_cached(options: Options, parent: &Path, cached: Option<&str>) -> Result<Self> {
        let mut job_options = options.clone();
        if job_options.device.is_none() {
            job_options.device = cached.map(str::to_owned);
        }
        let job = Job::start(&job_options, parent)?;
        let progress = job.shared.lock().unwrap().progress.clone();
        Ok(Self {
            options,
            job: Some(job),
            progress,
            error: None,
            cancelling: false,
            pages_added: 0,
            ready: false,
        })
    }
}

impl Job {
    pub fn start(options: &Options, parent: &Path) -> Result<Self> {
        Self::start_with(
            options,
            parent,
            Path::new("scanimage"),
            std::env::var("PDFVIEW_SCAN_COMMAND").ok(),
        )
    }

    pub(super) fn start_with(
        options: &Options,
        parent: &Path,
        program: &Path,
        custom: Option<String>,
    ) -> Result<Self> {
        options.validate()?;
        let temp = tempfile::Builder::new()
            .prefix("scan-")
            .tempdir_in(parent)?;
        let output = temp.path().join("page.pdf");
        let mut initial = Shared::default();
        if options.device.is_some() {
            initial.progress.message =
                format!("Opening scanner · {} DPI · {}", options.dpi, options.mode);
        }
        let shared = Arc::new(Mutex::new(initial));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (worker_shared, worker_cancelled) = (shared.clone(), cancelled.clone());
        let (options, worker_output, program) =
            (options.clone(), output.clone(), program.to_path_buf());
        let worker = thread::spawn(move || {
            let result = scan(
                &options,
                &worker_output,
                &program,
                custom.as_deref(),
                &worker_cancelled,
                &worker_shared,
            );
            let outcome = if worker_cancelled.load(Ordering::Relaxed) {
                Outcome::Cancelled
            } else {
                match result {
                    Ok(()) => Outcome::Done,
                    Err(error) => Outcome::Failed(format!("{error:#}")),
                }
            };
            worker_shared.lock().unwrap().outcome = Some(outcome);
        });
        Ok(Self {
            output,
            shared,
            cancelled,
            worker: Some(worker),
            _temp: temp,
        })
    }

    pub fn poll(&self) -> (Progress, Option<Outcome>) {
        let mut shared = self.shared.lock().unwrap();
        (shared.progress.clone(), shared.outcome.take())
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn choose_device(text: &str) -> Result<String> {
    let devices: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("v4l:"))
        .collect();
    match devices.as_slice() {
        [device] => Ok((*device).into()),
        [] => bail!("No scanner found. Check power and connection, then press Enter to retry."),
        _ => bail!(
            "Multiple scanners found. Set scanning.device in :config:\n{}",
            devices.join("\n")
        ),
    }
}

fn scan(
    options: &Options,
    output: &Path,
    program: &Path,
    custom: Option<&str>,
    cancelled: &AtomicBool,
    shared: &Arc<Mutex<Shared>>,
) -> Result<()> {
    let mut command;
    if let Some(custom) = custom {
        shared.lock().unwrap().progress.message = "Running custom scan command…".into();
        command = Command::new("sh");
        command
            .args(["-c", custom, "pdfview-scan"])
            .arg(output)
            .arg(options.dpi.to_string());
    } else {
        let device = if let Some(device) = &options.device {
            device.clone()
        } else {
            let bytes = run_command(
                Command::new(program).args(["--formatted-device-list", "%d%n"]),
                cancelled,
                shared,
                Duration::from_secs(15),
                false,
            )?;
            choose_device(&String::from_utf8_lossy(&bytes))?
        };
        {
            let mut shared = shared.lock().unwrap();
            shared.progress.message = format!("Scanning · {} DPI · {}", options.dpi, options.mode);
            shared.progress.device = Some(device.clone());
        }
        command = Command::new(program);
        command
            .arg("--device-name")
            .arg(device)
            .args(["--format=pdf", "--progress", "--mode"])
            .arg(&options.mode)
            .arg("--resolution")
            .arg(options.dpi.to_string())
            .arg("--output-file")
            .arg(output);
        if let Some(source) = &options.source {
            command.arg("--source").arg(source);
        }
    }
    run_command(
        &mut command,
        cancelled,
        shared,
        Duration::from_secs(options.timeout_seconds),
        true,
    )?;
    let file = std::fs::File::open(output).context("scanner did not create a PDF")?;
    let mut header = [0; 5];
    (&file)
        .read_exact(&mut header)
        .context("scanner returned an empty or incomplete PDF")?;
    if &header != b"%PDF-" {
        bail!("scanner output is not a PDF");
    }
    Ok(())
}

fn signal_group(pid: u32, signal: i32) {
    // Each child has its own process group; never signal pdfview's terminal group.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

fn read_pipe(
    mut pipe: impl Read + Send + 'static,
    progress: Option<Arc<Mutex<Shared>>>,
) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut line = Vec::new();
        let mut chunk = [0; 2048];
        while let Ok(count) = pipe.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let keep = count.min(65536_usize.saturating_sub(captured.len()));
            captured.extend_from_slice(&chunk[..keep]);
            if let Some(shared) = &progress {
                for byte in &chunk[..count] {
                    if matches!(byte, b'\r' | b'\n') {
                        let text = String::from_utf8_lossy(&line);
                        if let Some(percent) = parse_progress(&text) {
                            let mut shared = shared.lock().unwrap();
                            shared.progress.percent = Some(percent);
                        }
                        line.clear();
                    } else if line.len() < 1024 {
                        line.push(*byte);
                    }
                }
            }
        }
        captured
    })
}

fn parse_progress(text: &str) -> Option<f64> {
    let raw = text.strip_prefix("Progress: ")?.trim().strip_suffix('%')?;
    raw.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(0.0, 100.0))
}

fn run_command(
    command: &mut Command,
    cancelled: &AtomicBool,
    shared: &Arc<Mutex<Shared>>,
    timeout: Duration,
    progress: bool,
) -> Result<Vec<u8>> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("Scan cancelled");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .process_group(0)
        .spawn()
        .context("start scanner command (install SANE scanimage if missing)")?;
    let pid = child.id();
    let stdout = read_pipe(child.stdout.take().unwrap(), None);
    let stderr = read_pipe(
        child.stderr.take().unwrap(),
        progress.then(|| shared.clone()),
    );
    let deadline = Instant::now() + timeout;
    let mut stopping = None;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        if stopping.is_none() && (cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline) {
            timed_out = !cancelled.load(Ordering::Relaxed);
            stopping = Some(Instant::now());
            signal_group(pid, libc::SIGTERM);
        }
        if stopping.is_some_and(|at| at.elapsed() >= Duration::from_millis(500)) {
            signal_group(pid, libc::SIGKILL);
            let _ = child.kill();
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Also close inherited pipes if a custom command left helper processes behind.
    signal_group(pid, libc::SIGKILL);
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if cancelled.load(Ordering::Relaxed) {
        bail!("Scan cancelled");
    }
    if timed_out {
        bail!(
            "Scanner timed out after {} seconds. Check the connection, then retry.",
            timeout.as_secs()
        );
    }
    let status = status.context("wait for scanner")?;
    if !status.success() {
        let detail: String = String::from_utf8_lossy(&stderr)
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(2000)
            .collect();
        bail!("Scan failed ({status}): {}", detail.trim());
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake(parent: &Path, text: &str) -> Result<PathBuf> {
        let path = parent.join("scanimage");
        std::fs::write(&path, format!("#!/bin/sh\n{text}\n"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(path)
    }

    fn finish(job: &Job) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let (_, Some(outcome)) = job.poll() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "scanner worker did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn discovery_ignores_cameras_and_does_not_guess_between_scanners() -> Result<()> {
        assert_eq!(
            choose_device("v4l:/dev/video0\nhpaio:/usb/HP\n")?,
            "hpaio:/usb/HP"
        );
        assert!(choose_device("v4l:/dev/video0\n").is_err());
        assert!(choose_device("one\ntwo\n").is_err());
        assert_eq!(parse_progress("Progress: 42.5%"), Some(42.5));
        assert_eq!(parse_progress("Progress: NaN%"), None);
        Ok(())
    }

    #[test]
    fn cached_device_expires_after_two_minutes() {
        let mut cache = CachedDevice::remember("hpaio:/usb/test".into());
        assert_eq!(cache.device(), Some("hpaio:/usb/test"));
        cache.last_used = Instant::now() - DEVICE_CACHE_DURATION;
        assert_eq!(cache.device(), None);
    }

    #[test]
    fn native_scan_uses_literal_arguments_and_private_temporary_output() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let fake = fake(parent.path(), "[ \"$1\" = --formatted-device-list ] && { printf 'hpaio:/usb/test\\n'; exit; }\nwhile [ $# -gt 0 ]; do case \"$1\" in --output-file) shift; output=$1;; esac; shift; done\nprintf 'Progress: 50.0%%\\rProgress: 100.0%%\\n' >&2\nprintf '%%PDF-1.7\\nfixture' > \"$output\"")?;
        let job = Job::start_with(&Options::default(), parent.path(), &fake, None)?;
        assert!(matches!(finish(&job), Outcome::Done));
        assert_eq!(job.poll().0.percent, Some(100.0));
        let output = job.output.clone();
        assert!(output.exists());
        drop(job);
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn cancelling_reaps_command_and_helpers_and_allows_retry() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let fake = fake(parent.path(), "sleep 30 &\nwait")?;
        let options = Options {
            device: Some("test".into()),
            ..Options::default()
        };
        let job = Job::start_with(&options, parent.path(), &fake, None)?;
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        job.cancel();
        assert!(matches!(finish(&job), Outcome::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(job);
        let fake = self::fake(parent.path(), "printf 'device busy\\n' >&2\nexit 3")?;
        let job = Job::start_with(&options, parent.path(), &fake, None)?;
        assert!(
            matches!(finish(&job), Outcome::Failed(message) if message.contains("device busy"))
        );
        Ok(())
    }

    #[test]
    fn timeouts_and_non_pdf_output_are_failures() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let fake = fake(parent.path(), "trap '' TERM\nsleep 30 &\nwait")?;
        let options = Options {
            device: Some("test".into()),
            timeout_seconds: 1,
            ..Options::default()
        };
        let job = Job::start_with(&options, parent.path(), &fake, None)?;
        assert!(matches!(finish(&job), Outcome::Failed(message) if message.contains("timed out")));
        drop(job);
        let fake = self::fake(parent.path(), "while [ $# -gt 0 ]; do case \"$1\" in --output-file) shift; output=$1;; esac; shift; done\nprintf 'not a pdf' > \"$output\"")?;
        let job = Job::start_with(&options, parent.path(), &fake, None)?;
        assert!(matches!(finish(&job), Outcome::Failed(message) if message.contains("not a PDF")));
        Ok(())
    }
}
