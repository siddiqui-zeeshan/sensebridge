//! What the engine reported, written to a file for when something goes wrong.
//!
//! Logs are kept short on purpose: one file per day, the last few days only, and each
//! file capped in size. Older files are deleted when the app starts and at midnight.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Days of logs kept, today included.
pub const KEEP_DAYS: i64 = 3;
/// A day's file starts again from empty when it reaches this size.
pub const MAX_BYTES: u64 = 512 * 1024;

const PREFIX: &str = "sensebridge-";
const SUFFIX: &str = ".log";

/// Local date and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Stamp {
    pub fn now() -> Self {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTime {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            milliseconds: u16,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLocalTime(time: *mut SystemTime);
        }
        let mut time = SystemTime::default();
        // SAFETY: GetLocalTime fills the SYSTEMTIME it is given and cannot fail.
        unsafe { GetLocalTime(&mut time) };
        Self {
            year: time.year.into(),
            month: time.month.into(),
            day: time.day.into(),
            hour: time.hour.into(),
            minute: time.minute.into(),
            second: time.second.into(),
        }
    }

    fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Days since 1970-01-01, for telling how old a file is.
    fn day_number(&self) -> i64 {
        day_number(self.year, self.month, self.day)
    }
}

/// Days since 1970-01-01 in the Gregorian calendar.
fn day_number(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5
        + i64::from(day)
        - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The day a log file is for, from its name.
fn file_day(name: &str) -> Option<i64> {
    let date = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    parts.next().is_none().then(|| day_number(year, month, day))
}

/// Delete log files from before the last [`KEEP_DAYS`] days. Other files are left alone.
fn prune(dir: &Path, today: &Stamp) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let today = today.day_number();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(day) = name.to_str().and_then(file_day) else { continue };
        if day > today || today - day >= KEEP_DAYS {
            let _ = fs::remove_file(entry.path());
        }
    }
}

struct Open {
    date: String,
    file: File,
    size: u64,
}

pub struct Logger {
    dir: PathBuf,
    clock: Box<dyn Fn() -> Stamp + Send + Sync>,
    open: Mutex<Option<Open>>,
}

impl Logger {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_clock(dir, Box::new(Stamp::now))
    }

    pub fn with_clock(dir: PathBuf, clock: Box<dyn Fn() -> Stamp + Send + Sync>) -> Self {
        prune(&dir, &clock());
        Self { dir, clock, open: Mutex::new(None) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Add a line. Failing to log is never an error for the caller.
    pub fn line(&self, text: &str) {
        let now = (self.clock)();
        let date = now.date();
        let mut open = self.open.lock().unwrap();
        if open.as_ref().is_none_or(|o| o.date != date) {
            if open.is_some() {
                prune(&self.dir, &now); // a new day began while running
            }
            *open = self.open_file(&date);
        }
        let Some(current) = open.as_mut() else { return };
        let line =
            format!("{date} {:02}:{:02}:{:02}  {text}\n", now.hour, now.minute, now.second);
        if current.size + line.len() as u64 > MAX_BYTES {
            // Start the file again; an append-only handle cannot shorten it.
            let note = "(earlier entries today were dropped to keep this file small)
";
            *open = None;
            if fs::write(self.path(&date), note).is_ok() {
                *open = self.open_file(&date);
            }
        }
        let Some(current) = open.as_mut() else { return };
        if current.file.write_all(line.as_bytes()).is_ok() {
            current.size += line.len() as u64;
        }
    }

    fn path(&self, date: &str) -> PathBuf {
        self.dir.join(format!("{PREFIX}{date}{SUFFIX}"))
    }

    fn open_file(&self, date: &str) -> Option<Open> {
        fs::create_dir_all(&self.dir).ok()?;
        let file = OpenOptions::new().create(true).append(true).open(self.path(date)).ok()?;
        let size = file.metadata().ok()?.len();
        Some(Open { date: date.to_string(), file, size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("sensebridge-log-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn on(year: i32, month: u32, day: u32) -> Stamp {
        Stamp { year, month, day, hour: 9, minute: 5, second: 7 }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn day_numbers_follow_the_calendar() {
        assert_eq!(day_number(1970, 1, 1), 0);
        assert_eq!(day_number(2024, 3, 1) - day_number(2024, 2, 28), 2); // leap year
        assert_eq!(day_number(2026, 1, 1) - day_number(2025, 12, 31), 1);
    }

    #[test]
    fn lines_carry_the_local_time() {
        let dir = temp_dir("lines");
        let logger = Logger::with_clock(dir.clone(), Box::new(|| on(2026, 10, 11)));
        logger.line("controller lost");
        logger.line("controller is back");
        let text = fs::read_to_string(dir.join("sensebridge-2026-10-11.log")).unwrap();
        assert_eq!(
            text,
            "2026-10-11 09:05:07  controller lost\n2026-10-11 09:05:07  controller is back\n"
        );
    }

    #[test]
    fn starting_deletes_logs_older_than_three_days_and_nothing_else() {
        let dir = temp_dir("prune");
        for name in [
            "sensebridge-2026-10-11.log",
            "sensebridge-2026-10-09.log",
            "sensebridge-2026-10-08.log",
            "sensebridge-2025-01-01.log",
            "sensebridge-2027-01-01.log", // from a clock that was wrong
            "settings.json",
            "sensebridge-notes.log",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        Logger::with_clock(dir.clone(), Box::new(|| on(2026, 10, 11)));
        assert_eq!(
            names(&dir),
            [
                "sensebridge-2026-10-09.log",
                "sensebridge-2026-10-11.log",
                "sensebridge-notes.log",
                "settings.json",
            ]
        );
    }

    #[test]
    fn a_new_day_gets_a_new_file_and_the_oldest_goes() {
        let dir = temp_dir("rollover");
        let day = Arc::new(AtomicU32::new(28));
        let clock = {
            let day = Arc::clone(&day);
            Box::new(move || on(2026, 2, day.load(Ordering::Relaxed)))
        };
        fs::write(dir.join("sensebridge-2026-02-26.log"), "x").unwrap();
        let logger = Logger::with_clock(dir.clone(), clock);
        logger.line("before midnight");
        assert_eq!(names(&dir), ["sensebridge-2026-02-26.log", "sensebridge-2026-02-28.log"]);

        let logger = Logger { clock: Box::new(|| on(2026, 3, 1)), ..logger };
        logger.line("after midnight");
        assert_eq!(names(&dir), ["sensebridge-2026-02-28.log", "sensebridge-2026-03-01.log"]);
    }

    #[test]
    fn a_file_never_grows_past_its_cap() {
        let dir = temp_dir("cap");
        let logger = Logger::with_clock(dir.clone(), Box::new(|| on(2026, 10, 11)));
        let long = "x".repeat(1000);
        for _ in 0..(2 * MAX_BYTES / 1000) {
            logger.line(&long);
        }
        logger.line("the latest line");
        let path = dir.join("sensebridge-2026-10-11.log");
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        let text = fs::read_to_string(path).unwrap();
        assert!(text.starts_with("(earlier entries"));
        assert!(text.ends_with("the latest line\n"));
    }
}
