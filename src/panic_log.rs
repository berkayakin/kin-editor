//! Panics written to `<config>/logs/panic-<time>.log`: a panic in a GPUI
//! task aborts the app on macOS (its dispatcher doesn't unwind), and Kuana
//! started from the Dock has no terminal for the message to go to.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Writes every panic to a log in `dir` (its message, where it happened and
/// a backtrace), then reports it as before: the hook it replaces runs too.
pub fn install(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let thread = std::thread::current();
        match write(&dir, SystemTime::now(), thread.name().unwrap_or("<unnamed>"), &info.to_string(), &backtrace) {
            Ok(path) => eprintln!("kuana: the panic is written to {}", path.display()),
            Err(err) => eprintln!("kuana: couldn't write the panic to {}: {err}", dir.display()),
        }
        previous(info);
    }));
}

/// Writes one panic (`panic`: "panicked at <place>:\n<message>") to its log
/// in `dir`, after any other in the same second; returns the log's path.
fn write(dir: &Path, at: SystemTime, thread: &str, panic: &str, backtrace: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("panic-{}.log", stamp(at)));
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    writeln!(log, "Kuana {} on {os} {arch}", crate::about::version())?;
    writeln!(log, "thread '{thread}' {panic}\n\nBacktrace:\n{backtrace}")?;
    Ok(path)
}

/// `at` in UTC as `YYYY-MM-DD-HHMMSS`, so logs sort by time.
fn stamp(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, time) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970 to a date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let (era, day_of_era) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}-{:02}{:02}{:02}", time / 3600, time / 60 % 60, time % 60)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn logs_are_named_by_their_time() {
        let at = |secs: u64| stamp(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01-000000");
        assert_eq!(at(951_868_800), "2000-03-01-000000");
        assert_eq!(at(1_709_251_199), "2024-02-29-235959");
        assert_eq!(at(1_792_255_025), "2026-10-17-163705");
    }

    #[test]
    fn a_panic_goes_to_its_log() {
        // A folder of the test's own, never the user's config.
        let dir = std::env::temp_dir().join(format!("kuana-panic-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let at = UNIX_EPOCH + Duration::from_secs(1_792_255_025);
        let path = write(&dir, at, "main", "panicked at src/a.rs:1:2:\nboom", "0: kuana::a").unwrap();
        assert_eq!(path, dir.join("panic-2026-10-17-163705.log"));
        // Another in the same second goes after it.
        write(&dir, at, "worker", "panicked at src/b.rs:3:4:\nagain", "0: kuana::b").unwrap();
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.starts_with(&format!("Kuana {}", crate::about::version())), "{log}");
        assert!(log.contains("thread 'main' panicked at src/a.rs:1:2:\nboom\n\nBacktrace:\n0: kuana::a\n"), "{log}");
        assert!(log.contains("thread 'worker' panicked at src/b.rs:3:4:\nagain"), "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hook is the whole process's, so it's tried in a test process of
    /// its own: a panic leaves its log, and the hook before still reports it.
    #[test]
    fn the_hook_logs_a_panic_and_still_reports_it() {
        let dir = std::env::temp_dir().join(format!("kuana-panic-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "panic_log::tests::panics_with_the_hook", "--ignored", "--nocapture", "--test-threads=1"])
            .env("KUANA_PANIC_LOG_TEST", &dir)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(stderr.contains("on purpose") && stderr.contains("the panic is written to"), "{stderr}");
        let logs: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
        let [log] = logs.as_slice() else { panic!("{logs:?}") };
        let log = std::fs::read_to_string(log).unwrap();
        // The location as the platform writes it (`src\panic_log.rs` on Windows).
        let location = std::path::Path::new("src").join("panic_log.rs");
        assert!(log.contains(&format!("thread 'panic_log::tests::panics_with_the_hook' panicked at {}:", location.display())), "{log}");
        assert!(log.contains("on purpose\n\nBacktrace:\n"), "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Run by the test above, in its own process.
    #[test]
    #[ignore = "run by the_hook_logs_a_panic_and_still_reports_it"]
    fn panics_with_the_hook() {
        let Some(dir) = std::env::var_os("KUANA_PANIC_LOG_TEST") else { return };
        install(PathBuf::from(dir));
        panic!("on purpose");
    }
}
