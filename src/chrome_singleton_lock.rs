use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use nix::errno::Errno;
use nix::unistd::Pid;

const SINGLETON_LOCK_FILE: &str = "SingletonLock";

#[derive(Debug, PartialEq)]
pub enum SingletonLockState {
    Absent,
    Stale,
    Held,
}

// Chrome points its SingletonLock symlink at "<hostname>-<pid>".
#[derive(Debug, PartialEq)]
enum SingletonLockOwner {
    ThisHost { process_id: i32 },
    OtherHost,
    Unrecognized,
}

#[derive(Debug, PartialEq)]
enum ProcessLiveness {
    Alive,
    NotRunning,
}

pub fn read_singleton_lock_state(profile_dir: &Path) -> SingletonLockState {
    let lock_path: PathBuf = profile_dir.join(SINGLETON_LOCK_FILE);
    let metadata_result: std::io::Result<std::fs::Metadata> = std::fs::symlink_metadata(&lock_path);

    match metadata_result {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SingletonLockState::Absent,
        Err(_) => return SingletonLockState::Held,
    }

    let lock_owner: SingletonLockOwner = read_singleton_lock_owner(&lock_path);

    match lock_owner {
        SingletonLockOwner::ThisHost { process_id } => match probe_process_liveness(process_id) {
            ProcessLiveness::Alive => SingletonLockState::Held,
            ProcessLiveness::NotRunning => SingletonLockState::Stale,
        },
        SingletonLockOwner::OtherHost | SingletonLockOwner::Unrecognized => SingletonLockState::Held,
    }
}

fn read_singleton_lock_owner(lock_path: &Path) -> SingletonLockOwner {
    let link_target_result: std::io::Result<PathBuf> = std::fs::read_link(lock_path);
    let host_name_result: nix::Result<OsString> = nix::unistd::gethostname();

    let (Ok(link_target), Ok(host_name)) = (link_target_result, host_name_result) else {
        return SingletonLockOwner::Unrecognized;
    };

    let (Some(link_target), Some(host_name)) = (link_target.to_str(), host_name.to_str()) else {
        return SingletonLockOwner::Unrecognized;
    };

    parse_singleton_lock_target(link_target, host_name)
}

fn parse_singleton_lock_target(link_target: &str, this_host_name: &str) -> SingletonLockOwner {
    let Some((lock_host_name, process_id_text)) = link_target.rsplit_once('-') else {
        return SingletonLockOwner::Unrecognized;
    };

    let parsed_process_id: Option<i32> = process_id_text.parse::<i32>().ok().filter(|process_id| *process_id > 0);

    let Some(process_id) = parsed_process_id else {
        return SingletonLockOwner::Unrecognized;
    };

    match lock_host_name == this_host_name {
        true => SingletonLockOwner::ThisHost { process_id },
        false => SingletonLockOwner::OtherHost,
    }
}

// Signal 0 checks for the process without signalling it; EPERM means it exists under another user.
fn probe_process_liveness(process_id: i32) -> ProcessLiveness {
    let probe_result: nix::Result<()> = nix::sys::signal::kill(Pid::from_raw(process_id), None);

    match probe_result {
        Err(Errno::ESRCH) => ProcessLiveness::NotRunning,
        Ok(()) | Err(_) => ProcessLiveness::Alive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_singleton_lock_target_reads_the_process_id_on_this_host() {
        let lock_owner: SingletonLockOwner = parse_singleton_lock_target("mac-mini.local-4321", "mac-mini.local");

        assert_eq!(lock_owner, SingletonLockOwner::ThisHost { process_id: 4321 });
    }

    #[test]
    fn parse_singleton_lock_target_reports_another_host() {
        let lock_owner: SingletonLockOwner = parse_singleton_lock_target("server.example-4321", "mac-mini.local");

        assert_eq!(lock_owner, SingletonLockOwner::OtherHost);
    }

    #[test]
    fn parse_singleton_lock_target_rejects_a_target_without_a_positive_process_id() {
        let lock_owners: Vec<SingletonLockOwner> = ["mac-mini", "mac-mini-0", "mac-mini-"]
            .into_iter()
            .map(|link_target| parse_singleton_lock_target(link_target, "mac-mini"))
            .collect();

        assert_eq!(
            lock_owners,
            vec![
                SingletonLockOwner::Unrecognized,
                SingletonLockOwner::Unrecognized,
                SingletonLockOwner::Unrecognized,
            ],
        );
    }

    #[test]
    fn probe_process_liveness_finds_the_current_process() {
        let current_process_id: i32 = i32::try_from(std::process::id()).unwrap();

        assert_eq!(probe_process_liveness(current_process_id), ProcessLiveness::Alive);
    }
}
