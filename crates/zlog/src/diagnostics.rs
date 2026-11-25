use std::{
    fmt, fs,
    fmt::Write as _,
    io::{self, Write},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static WRITE_LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A small, independently retained flight record. No file contents or credentials belong here.
pub struct DiagnosticSpan {
    id: u64,
    operation: &'static str,
    start: Instant,
    finished: bool,
}

impl DiagnosticSpan {
    pub fn new(operation: &'static str) -> Self {
        let span = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            operation,
            start: Instant::now(),
            finished: false,
        };
        span.record(format_args!("begin"));
        span
    }

    pub fn record(&self, details: fmt::Arguments<'_>) {
        let Some(log_path) = crate::sink::output_file_path() else {
            return;
        };
        let path = log_path.with_extension("diagnostics.log");
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let result = append_record(
            &path,
            MAX_BYTES,
            format_args!(
                "{} pid={} id={} operation={} elapsed_ms={} {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
                std::process::id(),
                self.id,
                self.operation,
                self.start.elapsed().as_millis(),
                details
            ),
        );
        if let Err(error) = result {
            eprintln!("Failed to write diagnostics {}: {error}", path.display());
        }
    }

    pub fn finish(mut self) {
        self.record(format_args!("complete"));
        self.finished = true;
    }
}

impl Drop for DiagnosticSpan {
    fn drop(&mut self) {
        if !self.finished {
            self.record(format_args!(
                "incomplete panicking={}",
                std::thread::panicking()
            ));
        }
    }
}

/// An open diagnostics file reused between records.
///
/// Every checkpoint used to re-stat and re-open the file (one `metadata`, one
/// `open`, one `fsync` per record, three records per restored workspace item),
/// which is slow on Windows and on scanned or network home directories.
struct OpenLog {
    path: std::path::PathBuf,
    file: fs::File,
    bytes: u64,
}

static OPEN_LOG: Mutex<Option<OpenLog>> = Mutex::new(None);

fn append_record(path: &Path, max_bytes: u64, details: fmt::Arguments<'_>) -> io::Result<()> {
    let mut open = OPEN_LOG
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if open.as_ref().is_some_and(|log| log.path != path) {
        *open = None;
    }
    if open.is_none() {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let bytes = file.metadata()?.len();
        *open = Some(OpenLog {
            path: path.to_path_buf(),
            file,
            bytes,
        });
    }
    // The rotation check must also cover a freshly opened file: the previous
    // record may have been written before another thread switched the handle to
    // a different diagnostics path.
    if open.as_ref().is_some_and(|log| log.bytes >= max_bytes) {
        *open = None;
        // Copy before truncating so a failed rotation never destroys the previous evidence.
        fs::copy(path, path.with_extension("log.old"))?;
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        *open = Some(OpenLog {
            path: path.to_path_buf(),
            file,
            bytes: 0,
        });
    }
    let log = open.as_mut().expect("diagnostics file is open");
    let mut line = String::new();
    writeln!(line, "{details}")
        .map_err(|error| io::Error::other(format!("formatting diagnostics record: {error}")))?;
    log.file.write_all(line.as_bytes())?;
    log.bytes += line.len() as u64;
    // File::flush does not force the OS cache out. Checkpoints must survive an abrupt process exit.
    log.file.sync_data()
}

/// Forces the cached diagnostics file to disk and closes it, so an external
/// reader (or a harness that inspects the file after the process ends) sees
/// every record without relying on the handle's drop order.
pub fn flush_diagnostics() {
    let mut open = OPEN_LOG.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(log) = open.as_mut() {
        if let Err(error) = log.file.sync_data() {
            eprintln!("Failed to flush diagnostics: {error}");
        }
    }
    *open = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_immediately_readable_and_retained_on_rotation() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("Zed.diagnostics.log");
        append_record(&path, 10, format_args!("begin id=1"))?;
        assert_eq!(fs::read_to_string(&path)?, "begin id=1\n");
        append_record(&path, 10, format_args!("complete id=1"))?;
        assert_eq!(
            fs::read_to_string(path.with_extension("log.old"))?,
            "begin id=1\n"
        );
        assert_eq!(fs::read_to_string(&path)?, "complete id=1\n");
        Ok(())
    }

    #[test]
    fn spans_record_success_cancellation_and_panic_without_a_logger() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = Box::leak(Box::new(directory.path().join("Zed.log")));
        crate::init_output_file(path, None)?;
        DiagnosticSpan::new("success").finish();
        drop(DiagnosticSpan::new("cancelled"));
        assert!(
            std::panic::catch_unwind(|| {
                let _span = DiagnosticSpan::new("panic");
                panic!("diagnostic regression");
            })
            .is_err()
        );
        let contents = fs::read_to_string(path.with_extension("diagnostics.log"))?;
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(lines[1].contains("operation=success") && lines[1].ends_with("complete"));
        assert!(lines[3].ends_with("incomplete panicking=false"));
        assert!(lines[5].ends_with("incomplete panicking=true"));
        Ok(())
    }

    #[test]
    fn failed_rotation_preserves_evidence() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("Zed.diagnostics.log");
        append_record(&path, 1, format_args!("begin"))?;
        fs::create_dir(path.with_extension("log.old"))?;
        assert!(append_record(&path, 1, format_args!("complete")).is_err());
        assert_eq!(fs::read_to_string(path)?, "begin\n");
        Ok(())
    }
}
