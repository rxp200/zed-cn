use std::{
    fmt, fs,
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

fn append_record(path: &Path, max_bytes: u64, details: fmt::Arguments<'_>) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() >= max_bytes => {
            // Copy before truncating so a failed rotation never destroys the previous evidence.
            fs::copy(path, path.with_extension("log.old"))?;
            fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{details}")?;
    // File::flush does not force the OS cache out. Checkpoints must survive an abrupt process exit.
    file.sync_data()
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
