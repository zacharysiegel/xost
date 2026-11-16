use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::AppError;

const BROWSER_LOCK_FILE: &str = "browser.lock";
pub const POST_LOCK_WAIT: Duration = Duration::from_secs(10 * 60);
const LOCK_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// An exclusive advisory lock on an account's Chrome profile, held until released or dropped.
pub struct BrowserLock {
    lock_file: File,
}

impl BrowserLock {
    pub fn release(self) -> Result<(), AppError> {
        self.lock_file.unlock()?;

        Ok(())
    }
}

pub enum BrowserLockAcquisition {
    Acquired(BrowserLock),
    HeldElsewhere,
}

#[derive(Debug, PartialEq)]
enum HeldLockDecision {
    PollAgain,
    GiveUp,
}

pub fn get_lock_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name(BROWSER_LOCK_FILE)
}

pub async fn acquire_waiting(lock_path: &Path, max_wait: Duration) -> Result<BrowserLockAcquisition, AppError> {
    let start: tokio::time::Instant = tokio::time::Instant::now();

    loop {
        let acquisition: BrowserLockAcquisition = try_acquire(lock_path)?;

        if let BrowserLockAcquisition::Acquired(_) = acquisition {
            return Ok(acquisition);
        }

        let held_lock_decision: HeldLockDecision = decide_after_held_lock(start.elapsed(), max_wait);

        match held_lock_decision {
            HeldLockDecision::PollAgain => tokio::time::sleep(LOCK_POLL_INTERVAL).await,
            HeldLockDecision::GiveUp => return Ok(BrowserLockAcquisition::HeldElsewhere),
        }
    }
}

pub fn acquire_without_waiting(db_path: &Path) -> Result<BrowserLock, AppError> {
    let lock_path: PathBuf = get_lock_path(db_path);
    let acquisition: BrowserLockAcquisition = try_acquire(&lock_path)?;

    match acquisition {
        BrowserLockAcquisition::Acquired(browser_lock) => Ok(browser_lock),
        BrowserLockAcquisition::HeldElsewhere => Err(AppError::new(&format!(
            "another xost process is using this account's Chrome profile; it holds {}",
            lock_path.display(),
        ))),
    }
}

pub fn try_acquire(lock_path: &Path) -> Result<BrowserLockAcquisition, AppError> {
    let lock_file: File = open_lock_file(lock_path)?;

    let lock_result: Result<(), TryLockError> = lock_file.try_lock();

    match lock_result {
        Ok(()) => Ok(BrowserLockAcquisition::Acquired(BrowserLock { lock_file })),
        Err(TryLockError::WouldBlock) => Ok(BrowserLockAcquisition::HeldElsewhere),
        Err(TryLockError::Error(error)) => Err(AppError::new(&format!(
            "could not lock {}: {}",
            lock_path.display(),
            error,
        ))),
    }
}

fn open_lock_file(lock_path: &Path) -> Result<File, AppError> {
    if let Some(parent) = lock_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    let lock_file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;

    Ok(lock_file)
}

fn decide_after_held_lock(waited: Duration, max_wait: Duration) -> HeldLockDecision {
    match waited < max_wait {
        true => HeldLockDecision::PollAgain,
        false => HeldLockDecision::GiveUp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_lock_path(test_name: &str) -> PathBuf {
        let directory: PathBuf = std::env::temp_dir().join(format!("xost-browser-lock-{}-{}", test_name, std::process::id()));

        std::fs::create_dir_all(&directory).unwrap();

        directory.join(BROWSER_LOCK_FILE)
    }

    fn remove_lock_directory(lock_path: &Path) {
        if let Some(directory) = lock_path.parent() {
            std::fs::remove_dir_all(directory).ok();
        }
    }

    fn acquire_expecting_success(lock_path: &Path) -> BrowserLock {
        let BrowserLockAcquisition::Acquired(browser_lock) = try_acquire(lock_path).unwrap() else {
            panic!("the lock is held elsewhere");
        };

        browser_lock
    }

    #[test]
    fn get_lock_path_places_the_lock_beside_the_queue_db() {
        assert_eq!(get_lock_path(Path::new("/tmp/daily/queue.db")), PathBuf::from("/tmp/daily/browser.lock"));
    }

    #[test]
    fn decide_after_held_lock_polls_again_within_the_wait() {
        let decision: HeldLockDecision = decide_after_held_lock(Duration::from_secs(60), POST_LOCK_WAIT);

        assert_eq!(decision, HeldLockDecision::PollAgain);
    }

    #[test]
    fn decide_after_held_lock_gives_up_once_the_wait_has_elapsed() {
        let decision: HeldLockDecision = decide_after_held_lock(POST_LOCK_WAIT, POST_LOCK_WAIT);

        assert_eq!(decision, HeldLockDecision::GiveUp);
    }

    #[test]
    fn try_acquire_reports_a_lock_held_by_another_handle() {
        let lock_path: PathBuf = create_lock_path("held");
        let _held_lock: BrowserLock = acquire_expecting_success(&lock_path);

        let second_acquisition: BrowserLockAcquisition = try_acquire(&lock_path).unwrap();
        remove_lock_directory(&lock_path);

        assert!(matches!(second_acquisition, BrowserLockAcquisition::HeldElsewhere));
    }

    #[test]
    fn try_acquire_succeeds_after_the_holder_releases_the_lock() {
        let lock_path: PathBuf = create_lock_path("released");
        let first_lock: BrowserLock = acquire_expecting_success(&lock_path);

        first_lock.release().unwrap();

        let second_acquisition: BrowserLockAcquisition = try_acquire(&lock_path).unwrap();
        remove_lock_directory(&lock_path);

        assert!(matches!(second_acquisition, BrowserLockAcquisition::Acquired(_)));
    }
}
