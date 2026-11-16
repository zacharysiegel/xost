use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use crate::config;
use crate::config::Config;
use crate::cron::CronField;
use crate::cron::CronSchedule;
use crate::cron::DayRestriction;
use crate::error::AppError;

const LABEL_PREFIX: &str = "ro.zach.xost.";
const CHECK_LABEL_SUFFIX: &str = ".check";
const LAUNCH_DAEMONS_DIRECTORY: &str = "/Library/LaunchDaemons";
const SUDO_PATH: &str = "/usr/bin/sudo";
const LAUNCHCTL_PATH: &str = "/bin/launchctl";
const INSTALL_PATH: &str = "/usr/bin/install";
const RM_PATH: &str = "/bin/rm";
const ID_PATH: &str = "/usr/bin/id";
const ROOT_USER_NAME: &str = "root";
// launchctl exits with 113 ("Could not find service") when the job is not loaded.
const LAUNCHCTL_SERVICE_NOT_FOUND_EXIT_CODE: i32 = 113;
// launchctl bootout also exits with ESRCH (3) when the job is not loaded.
const BOOTOUT_NOT_LOADED_EXIT_CODES: [i32; 2] = [3, LAUNCHCTL_SERVICE_NOT_FOUND_EXIT_CODE];
// launchctl bootstrap exits with EIO (5) while a just-unloaded service with the same label is still being torn down.
const BOOTSTRAP_INPUT_OUTPUT_ERROR_EXIT_CODE: i32 = 5;
const MAX_BOOTSTRAP_ATTEMPTS: u32 = 5;
const BOOTSTRAP_RETRY_DELAY: Duration = Duration::from_secs(1);
const PLIST_HEADER: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
";
const PLIST_FOOTER: &str = "\
</dict>
</plist>
";

pub enum ScheduleAction {
    Install,
    Print,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JobKind {
    Post,
    Check,
}

impl JobKind {
    pub const ALL: [JobKind; 2] = [JobKind::Post, JobKind::Check];

    pub fn schedule_key(self) -> &'static str {
        match self {
            JobKind::Post => config::SCHEDULE_KEY,
            JobKind::Check => config::CHECK_SCHEDULE_KEY,
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            JobKind::Post => "post",
            JobKind::Check => "check",
        }
    }

    // Account names cannot contain '.', so these suffixes never collide with another account's names.
    fn name_suffix(self) -> &'static str {
        match self {
            JobKind::Post => "",
            JobKind::Check => CHECK_LABEL_SUFFIX,
        }
    }

    fn get_schedule(self, config: &Config) -> Option<&CronSchedule> {
        match self {
            JobKind::Post => config.schedule.as_ref(),
            JobKind::Check => config.check_schedule.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalendarInterval {
    pub minute: Option<u8>,
    pub hour: Option<u8>,
    pub day: Option<u8>,
    pub weekday: Option<u8>,
    pub month: Option<u8>,
}

impl CalendarInterval {
    fn unrestricted() -> CalendarInterval {
        CalendarInterval {
            minute: None,
            hour: None,
            day: None,
            weekday: None,
            month: None,
        }
    }

    fn plist_entries(&self) -> Vec<(&'static str, u8)> {
        let candidate_entries: [(&'static str, Option<u8>); 5] = [
            ("Minute", self.minute),
            ("Hour", self.hour),
            ("Day", self.day),
            ("Weekday", self.weekday),
            ("Month", self.month),
        ];

        candidate_entries
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect()
    }
}

pub struct LaunchDaemonJob {
    pub account: String,
    pub job_kind: JobKind,
    pub user_name: String,
    pub home_directory: String,
    pub executable_path: String,
    pub config_path: String,
    pub calendar_intervals: Vec<CalendarInterval>,
}

pub enum JobLoading {
    Loaded,
    NotLoaded,
    Unknown,
}

enum BootoutOutcome {
    Unloaded,
    WasNotLoaded,
    Failed,
}

#[derive(Debug, PartialEq)]
enum BootstrapDecision {
    Loaded,
    RetryAfterDelay,
    Fail,
}

#[derive(Debug, PartialEq)]
enum JobPresence {
    Present,
    Absent,
}

struct JobEnvironment {
    user_name: String,
    home_directory: String,
    executable_path: String,
    config_path: String,
}

pub fn run(config: &Config, config_path: &Path, action: ScheduleAction) -> Result<(), AppError> {
    let launch_daemon_jobs: Vec<LaunchDaemonJob> = create_launch_daemon_jobs(config, config_path)?;

    match action {
        ScheduleAction::Print => print_plists(&launch_daemon_jobs),
        ScheduleAction::Install => install(&launch_daemon_jobs, &config.account),
    }
}

fn create_launch_daemon_jobs(config: &Config, config_path: &Path) -> Result<Vec<LaunchDaemonJob>, AppError> {
    if config.schedule.is_none() {
        return Err(AppError::new(&format!(
            "account '{}' has no {} key; add one to its config section, e.g. {} = 0 9 * * *",
            config.account,
            config::SCHEDULE_KEY,
            config::SCHEDULE_KEY,
        )));
    }

    let job_environment: JobEnvironment = get_job_environment(config_path)?;
    let mut launch_daemon_jobs: Vec<LaunchDaemonJob> = Vec::new();

    for job_kind in JobKind::ALL {
        let Some(schedule) = job_kind.get_schedule(config)
        else {
            continue;
        };

        launch_daemon_jobs.push(create_launch_daemon_job(&config.account, job_kind, schedule, &job_environment));
    }

    Ok(launch_daemon_jobs)
}

fn print_plists(launch_daemon_jobs: &[LaunchDaemonJob]) -> Result<(), AppError> {
    for launch_daemon_job in launch_daemon_jobs {
        eprintln!("Label: {}", job_label(&launch_daemon_job.account, launch_daemon_job.job_kind));
        print!("{}", render_plist(launch_daemon_job));
    }

    Ok(())
}

fn install(launch_daemon_jobs: &[LaunchDaemonJob], account: &str) -> Result<(), AppError> {
    for launch_daemon_job in launch_daemon_jobs {
        install_job(launch_daemon_job)?;
    }

    for job_kind in JobKind::ALL {
        let job_requested: bool = launch_daemon_jobs.iter().any(|launch_daemon_job| launch_daemon_job.job_kind == job_kind);
        let job_present: JobPresence = read_job_presence(account, job_kind);

        if !job_requested && job_present == JobPresence::Present {
            remove_job_and_report(account, job_kind)?;
        }
    }

    Ok(())
}

fn install_job(job: &LaunchDaemonJob) -> Result<(), AppError> {
    let label: String = job_label(&job.account, job.job_kind);
    let installed_plist_path: String = installed_plist_path(&label);

    let temporary_plist_path: PathBuf = std::env::temp_dir().join(format!("{}.{}.plist", label, std::process::id()));

    std::fs::write(&temporary_plist_path, render_plist(job))?;

    let copy_result: Result<(), AppError> = copy_plist_into_launch_daemons(&temporary_plist_path, &installed_plist_path);
    std::fs::remove_file(&temporary_plist_path).ok();
    copy_result?;

    let bootout_outcome: BootoutOutcome = bootout(&label)?;
    bootstrap(&installed_plist_path, &bootout_outcome)?;

    println!("installed the LaunchDaemon");
    println!("Label: {}", label);
    println!("Plist: {}", installed_plist_path);
    println!("Log: {}", log_path(&job.home_directory, &job.account, job.job_kind));

    Ok(())
}

pub fn remove(account: &str) -> Result<(), AppError> {
    let mut removed_job_count: usize = 0;

    for job_kind in JobKind::ALL {
        let job_presence: JobPresence = read_job_presence(account, job_kind);

        if job_presence == JobPresence::Absent {
            continue;
        }

        remove_job_and_report(account, job_kind)?;
        removed_job_count += 1;
    }

    if removed_job_count == 0 {
        println!("no installed or loaded LaunchDaemon; nothing to remove");
        println!("Account: {}", account);
    }

    Ok(())
}

fn remove_job_and_report(account: &str, job_kind: JobKind) -> Result<(), AppError> {
    let label: String = job_label(account, job_kind);
    let installed_plist_path: String = installed_plist_path(&label);

    remove_job(&label, &installed_plist_path)?;

    println!("removed the LaunchDaemon");
    println!("Label: {}", label);
    println!("Plist: {}", installed_plist_path);

    Ok(())
}

pub fn remove_job(label: &str, installed_plist_path: &str) -> Result<(), AppError> {
    bootout(label)?;

    delete_installed_plist(installed_plist_path)
}

fn read_job_presence(account: &str, job_kind: JobKind) -> JobPresence {
    let label: String = job_label(account, job_kind);
    let plist_present: bool = Path::new(&installed_plist_path(&label)).exists();

    if plist_present {
        return JobPresence::Present;
    }

    let job_loading: JobLoading = read_job_loading(&label);

    match job_loading {
        JobLoading::NotLoaded => JobPresence::Absent,
        JobLoading::Loaded | JobLoading::Unknown => JobPresence::Present,
    }
}

fn get_job_environment(config_path: &Path) -> Result<JobEnvironment, AppError> {
    let user_name: String = invoking_user_name()?;

    if user_name == ROOT_USER_NAME {
        return Err(AppError::new(
            "run schedule as the user who owns the account, not as root; it calls sudo itself",
        ));
    }

    let home_directory: String =
        std::env::var("HOME").map_err(|_| AppError::new("HOME is not set; the job needs it as its home directory"))?;
    let executable_path: PathBuf = std::env::current_exe()?.canonicalize()?;
    let absolute_config_path: PathBuf = config_path.canonicalize()?;

    Ok(JobEnvironment {
        user_name,
        home_directory,
        executable_path: path_to_string(&executable_path)?,
        config_path: path_to_string(&absolute_config_path)?,
    })
}

fn create_launch_daemon_job(
    account: &str,
    job_kind: JobKind,
    schedule: &CronSchedule,
    job_environment: &JobEnvironment,
) -> LaunchDaemonJob {
    LaunchDaemonJob {
        account: account.to_string(),
        job_kind,
        user_name: job_environment.user_name.clone(),
        home_directory: job_environment.home_directory.clone(),
        executable_path: job_environment.executable_path.clone(),
        config_path: job_environment.config_path.clone(),
        calendar_intervals: calendar_intervals(schedule),
    }
}

fn invoking_user_name() -> Result<String, AppError> {
    let user_from_environment: Option<String> = std::env::var("USER").ok().filter(|user| !user.is_empty());

    if let Some(user) = user_from_environment {
        return Ok(user);
    }

    let output: Output = Command::new(ID_PATH).arg("-un").output()?;

    if !output.status.success() {
        return Err(AppError::new("USER is not set and `id -un` failed; cannot tell which user the job runs as"));
    }

    let user: String = String::from_utf8(output.stdout)?.trim().to_string();

    Ok(user)
}

fn path_to_string(path: &Path) -> Result<String, AppError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| AppError::new(&format!("path {} is not valid UTF-8", path.display())))
}

pub fn calendar_intervals(schedule: &CronSchedule) -> Vec<CalendarInterval> {
    let unrestricted: Vec<CalendarInterval> = vec![CalendarInterval::unrestricted()];
    let with_minutes: Vec<CalendarInterval> = expand(unrestricted, &schedule.minute, |interval, value| interval.minute = Some(value));
    let with_hours: Vec<CalendarInterval> = expand(with_minutes, &schedule.hour, |interval, value| interval.hour = Some(value));
    let with_months: Vec<CalendarInterval> = expand(with_hours, &schedule.month, |interval, value| interval.month = Some(value));

    expand_days(with_months, schedule)
}

fn expand_days(intervals: Vec<CalendarInterval>, schedule: &CronSchedule) -> Vec<CalendarInterval> {
    let assign_day: fn(&mut CalendarInterval, u8) = |interval, value| interval.day = Some(value);
    let assign_weekday: fn(&mut CalendarInterval, u8) = |interval, value| interval.weekday = Some(value);

    match schedule.day_restriction() {
        DayRestriction::EveryDay => intervals,
        DayRestriction::DayOfMonthOnly => expand(intervals, &schedule.day_of_month, assign_day),
        DayRestriction::DayOfWeekOnly => expand(intervals, &schedule.day_of_week, assign_weekday),
        DayRestriction::EitherDayField => {
            let by_day_of_month: Vec<CalendarInterval> = expand(intervals.clone(), &schedule.day_of_month, assign_day);
            let by_day_of_week: Vec<CalendarInterval> = expand(intervals, &schedule.day_of_week, assign_weekday);

            by_day_of_month.into_iter().chain(by_day_of_week).collect()
        }
    }
}

fn expand(
    intervals: Vec<CalendarInterval>,
    field: &CronField,
    assign: fn(&mut CalendarInterval, u8),
) -> Vec<CalendarInterval> {
    let Some(values) = field.restricted_values() else {
        return intervals;
    };

    let mut expanded_intervals: Vec<CalendarInterval> = Vec::with_capacity(intervals.len() * values.len());

    for interval in &intervals {
        for value in values {
            let mut expanded_interval: CalendarInterval = interval.clone();
            assign(&mut expanded_interval, *value);
            expanded_intervals.push(expanded_interval);
        }
    }

    expanded_intervals
}

pub fn render_plist(job: &LaunchDaemonJob) -> String {
    let label: String = job_label(&job.account, job.job_kind);
    let log_path: String = log_path(&job.home_directory, &job.account, job.job_kind);
    let program_arguments: [&str; 6] = [
        &job.executable_path,
        "--config",
        &job.config_path,
        "--account",
        &job.account,
        job.job_kind.subcommand(),
    ];

    let mut plist: String = String::from(PLIST_HEADER);

    push_string_entry(&mut plist, 1, "Label", &label);
    push_string_entry(&mut plist, 1, "UserName", &job.user_name);

    push_key(&mut plist, 1, "ProgramArguments");
    push_line(&mut plist, 1, "<array>");
    for argument in program_arguments {
        push_string(&mut plist, 2, argument);
    }
    push_line(&mut plist, 1, "</array>");

    push_key(&mut plist, 1, "EnvironmentVariables");
    push_line(&mut plist, 1, "<dict>");
    push_string_entry(&mut plist, 2, "HOME", &job.home_directory);
    push_line(&mut plist, 1, "</dict>");

    push_key(&mut plist, 1, "StartCalendarInterval");
    push_calendar_intervals(&mut plist, 1, &job.calendar_intervals);

    push_key(&mut plist, 1, "RunAtLoad");
    push_line(&mut plist, 1, "<false/>");

    push_string_entry(&mut plist, 1, "StandardOutPath", &log_path);
    push_string_entry(&mut plist, 1, "StandardErrorPath", &log_path);

    plist.push_str(PLIST_FOOTER);

    plist
}

fn push_calendar_intervals(plist: &mut String, depth: usize, calendar_intervals: &[CalendarInterval]) {
    push_line(plist, depth, "<array>");

    for calendar_interval in calendar_intervals {
        push_line(plist, depth + 1, "<dict>");

        for (key, value) in calendar_interval.plist_entries() {
            push_key(plist, depth + 2, key);
            push_line(plist, depth + 2, &format!("<integer>{}</integer>", value));
        }

        push_line(plist, depth + 1, "</dict>");
    }

    push_line(plist, depth, "</array>");
}

fn push_string_entry(plist: &mut String, depth: usize, key: &str, value: &str) {
    push_key(plist, depth, key);
    push_string(plist, depth, value);
}

fn push_key(plist: &mut String, depth: usize, key: &str) {
    push_line(plist, depth, &format!("<key>{}</key>", escape_xml(key)));
}

fn push_string(plist: &mut String, depth: usize, value: &str) {
    push_line(plist, depth, &format!("<string>{}</string>", escape_xml(value)));
}

fn push_line(plist: &mut String, depth: usize, text: &str) {
    plist.push_str(&"    ".repeat(depth));
    plist.push_str(text);
    plist.push('\n');
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn job_label(account: &str, job_kind: JobKind) -> String {
    format!("{}{}{}", LABEL_PREFIX, account, job_kind.name_suffix())
}

pub fn installed_plist_path(label: &str) -> String {
    format!("{}/{}.plist", LAUNCH_DAEMONS_DIRECTORY, label)
}

fn log_path(home_directory: &str, account: &str, job_kind: JobKind) -> String {
    Path::new(home_directory)
        .join("Library")
        .join("Logs")
        .join(format!("xost-{}{}.log", account, job_kind.name_suffix()))
        .display()
        .to_string()
}

fn copy_plist_into_launch_daemons(temporary_plist_path: &Path, installed_plist_path: &str) -> Result<(), AppError> {
    let status: ExitStatus = Command::new(SUDO_PATH)
        .arg(INSTALL_PATH)
        .args(["-m", "644", "-o", "root", "-g", "wheel"])
        .arg(temporary_plist_path)
        .arg(installed_plist_path)
        .status()?;

    require_success(status, &format!("copying the plist to {}", installed_plist_path))
}

fn bootstrap(installed_plist_path: &str, bootout_outcome: &BootoutOutcome) -> Result<(), AppError> {
    let mut attempt_number: u32 = 1;

    loop {
        let output: Output = run_bootstrap(installed_plist_path)?;
        let bootstrap_decision: BootstrapDecision = decide_after_bootstrap(output.status.code(), bootout_outcome, attempt_number);

        match bootstrap_decision {
            BootstrapDecision::Loaded => return Ok(()),
            BootstrapDecision::RetryAfterDelay => {
                println!(
                    "launchctl bootstrap failed while the previous job is still unloading; retrying (attempt {} of {})",
                    attempt_number + 1,
                    MAX_BOOTSTRAP_ATTEMPTS,
                );
                std::thread::sleep(BOOTSTRAP_RETRY_DELAY);

                attempt_number += 1;
            }
            BootstrapDecision::Fail => {
                return Err(AppError::new(&format!(
                    "launchctl bootstrap system {} failed on attempt {} of {} ({}): {}",
                    installed_plist_path,
                    attempt_number,
                    MAX_BOOTSTRAP_ATTEMPTS,
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim(),
                )));
            }
        }
    }
}

fn run_bootstrap(installed_plist_path: &str) -> Result<Output, AppError> {
    let output: Output = Command::new(SUDO_PATH)
        .arg(LAUNCHCTL_PATH)
        .args(["bootstrap", "system"])
        .arg(installed_plist_path)
        .stdin(Stdio::inherit())
        .output()?;

    Ok(output)
}

fn decide_after_bootstrap(exit_code: Option<i32>, bootout_outcome: &BootoutOutcome, attempt_number: u32) -> BootstrapDecision {
    let previous_job_may_still_be_unloading: bool = matches!(bootout_outcome, BootoutOutcome::Unloaded);
    let attempts_remain: bool = attempt_number < MAX_BOOTSTRAP_ATTEMPTS;

    match exit_code {
        Some(0) => BootstrapDecision::Loaded,
        Some(BOOTSTRAP_INPUT_OUTPUT_ERROR_EXIT_CODE) if previous_job_may_still_be_unloading && attempts_remain => {
            BootstrapDecision::RetryAfterDelay
        }
        _ => BootstrapDecision::Fail,
    }
}

fn delete_installed_plist(installed_plist_path: &str) -> Result<(), AppError> {
    let status: ExitStatus = Command::new(SUDO_PATH).arg(RM_PATH).arg("-f").arg(installed_plist_path).status()?;

    require_success(status, &format!("deleting {}", installed_plist_path))
}

pub fn read_job_loading(label: &str) -> JobLoading {
    let print_result: std::io::Result<ExitStatus> = Command::new(LAUNCHCTL_PATH)
        .arg("print")
        .arg(system_service_target(label))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let Ok(print_status) = print_result else {
        return JobLoading::Unknown;
    };

    match print_status.code() {
        Some(0) => JobLoading::Loaded,
        Some(LAUNCHCTL_SERVICE_NOT_FOUND_EXIT_CODE) => JobLoading::NotLoaded,
        _ => JobLoading::Unknown,
    }
}

fn system_service_target(label: &str) -> String {
    format!("system/{}", label)
}

fn bootout(label: &str) -> Result<BootoutOutcome, AppError> {
    let service_target: String = system_service_target(label);

    let output: Output = Command::new(SUDO_PATH)
        .arg(LAUNCHCTL_PATH)
        .arg("bootout")
        .arg(&service_target)
        .stdin(Stdio::inherit())
        .output()?;

    let bootout_outcome: BootoutOutcome = classify_bootout(output.status.code());

    match bootout_outcome {
        BootoutOutcome::Unloaded | BootoutOutcome::WasNotLoaded => Ok(bootout_outcome),
        BootoutOutcome::Failed => Err(AppError::new(&format!(
            "launchctl bootout {} failed ({}): {}",
            service_target,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ))),
    }
}

fn classify_bootout(exit_code: Option<i32>) -> BootoutOutcome {
    match exit_code {
        Some(0) => BootoutOutcome::Unloaded,
        Some(code) if BOOTOUT_NOT_LOADED_EXIT_CODES.contains(&code) => BootoutOutcome::WasNotLoaded,
        _ => BootoutOutcome::Failed,
    }
}

fn require_success(status: ExitStatus, description: &str) -> Result<(), AppError> {
    if !status.success() {
        return Err(AppError::new(&format!("{} failed ({})", description, status)));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_intervals_for(expression: &str) -> Vec<CalendarInterval> {
        calendar_intervals(&CronSchedule::parse(expression).unwrap())
    }

    fn get_calendar_interval_at(minute: Option<u8>, hour: Option<u8>, day: Option<u8>, weekday: Option<u8>) -> CalendarInterval {
        CalendarInterval {
            minute,
            hour,
            day,
            weekday,
            month: None,
        }
    }

    fn get_sample_job(account: &str, job_kind: JobKind) -> LaunchDaemonJob {
        LaunchDaemonJob {
            account: account.to_string(),
            job_kind,
            user_name: "zachary".to_string(),
            home_directory: "/Users/example".to_string(),
            executable_path: "/opt/xost/bin/xost".to_string(),
            config_path: "/Users/example/.config/xost/config".to_string(),
            calendar_intervals: vec![get_calendar_interval_at(Some(0), Some(9), None, None)],
        }
    }

    #[test]
    fn calendar_intervals_expands_a_minute_step() {
        let expected_intervals: Vec<CalendarInterval> = vec![
            get_calendar_interval_at(Some(0), Some(9), None, None),
            get_calendar_interval_at(Some(20), Some(9), None, None),
            get_calendar_interval_at(Some(40), Some(9), None, None),
        ];

        assert_eq!(get_intervals_for("*/20 9 * * *"), expected_intervals);
    }

    #[test]
    fn calendar_intervals_omits_every_wildcard_field() {
        assert_eq!(get_intervals_for("* * * * *"), vec![CalendarInterval::unrestricted()]);
    }

    #[test]
    fn calendar_intervals_unions_day_of_month_and_day_of_week_when_both_are_restricted() {
        let expected_intervals: Vec<CalendarInterval> = vec![
            get_calendar_interval_at(Some(0), Some(9), Some(1), None),
            get_calendar_interval_at(Some(0), Some(9), Some(15), None),
            get_calendar_interval_at(Some(0), Some(9), None, Some(0)),
        ];

        assert_eq!(get_intervals_for("0 9 1,15 * 7"), expected_intervals);
    }

    #[test]
    fn calendar_intervals_restricts_by_month_alongside_one_day_field() {
        let expected_intervals: Vec<CalendarInterval> = vec![
            CalendarInterval {
                minute: Some(0),
                hour: Some(9),
                day: None,
                weekday: Some(1),
                month: Some(6),
            },
            CalendarInterval {
                minute: Some(0),
                hour: Some(9),
                day: None,
                weekday: Some(1),
                month: Some(12),
            },
        ];

        assert_eq!(get_intervals_for("0 9 * 6,12 1"), expected_intervals);
    }

    #[test]
    fn render_plist_renders_a_launch_daemon_for_the_account() {
        let expected_plist: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
    <key>Label</key>
    <string>ro.zach.xost.daily</string>
    <key>UserName</key>
    <string>zachary</string>
    <key>ProgramArguments</key>
    <array>
        <string>/opt/xost/bin/xost</string>
        <string>--config</string>
        <string>/Users/example/.config/xost/config</string>
        <string>--account</string>
        <string>daily</string>
        <string>post</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>/Users/example</string>
    </dict>
    <key>StartCalendarInterval</key>
    <array>
        <dict>
            <key>Minute</key>
            <integer>0</integer>
            <key>Hour</key>
            <integer>9</integer>
        </dict>
    </array>
    <key>RunAtLoad</key>
    <false/>
    <key>StandardOutPath</key>
    <string>/Users/example/Library/Logs/xost-daily.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/example/Library/Logs/xost-daily.log</string>
</dict>
</plist>
";

        assert_eq!(render_plist(&get_sample_job("daily", JobKind::Post)), expected_plist);
    }

    #[test]
    fn render_plist_renders_the_check_job_with_its_own_label_subcommand_and_log() {
        let plist: String = render_plist(&get_sample_job("daily", JobKind::Check));

        let expected_fragments: [&str; 3] = [
            "    <key>Label</key>\n    <string>ro.zach.xost.daily.check</string>\n",
            "        <string>daily</string>\n        <string>check</string>\n    </array>\n",
            "    <key>StandardOutPath</key>\n    <string>/Users/example/Library/Logs/xost-daily.check.log</string>\n",
        ];
        let missing_fragments: Vec<&str> =
            expected_fragments.into_iter().filter(|expected_fragment| !plist.contains(expected_fragment)).collect();
        assert_eq!(missing_fragments, Vec::<&str>::new());
    }

    #[test]
    fn render_plist_escapes_xml_special_characters() {
        let mut job: LaunchDaemonJob = get_sample_job("daily", JobKind::Post);
        job.config_path = "/Users/example/<a & b> \"c\" 'd'".to_string();

        let plist: String = render_plist(&job);

        assert!(plist.contains("<string>/Users/example/&lt;a &amp; b&gt; &quot;c&quot; &apos;d&apos;</string>"));
    }

    #[test]
    fn classify_bootout_treats_a_missing_service_as_not_loaded() {
        assert!(matches!(classify_bootout(Some(3)), BootoutOutcome::WasNotLoaded));
        assert!(matches!(classify_bootout(Some(113)), BootoutOutcome::WasNotLoaded));
    }

    #[test]
    fn classify_bootout_treats_another_exit_code_as_a_failure() {
        assert!(matches!(classify_bootout(Some(1)), BootoutOutcome::Failed));
    }

    #[test]
    fn decide_after_bootstrap_retries_an_input_output_error_after_unloading_a_job() {
        let decision: BootstrapDecision = decide_after_bootstrap(Some(5), &BootoutOutcome::Unloaded, 1);

        assert_eq!(decision, BootstrapDecision::RetryAfterDelay);
    }

    #[test]
    fn decide_after_bootstrap_fails_an_input_output_error_when_no_job_was_unloaded() {
        let decision: BootstrapDecision = decide_after_bootstrap(Some(5), &BootoutOutcome::WasNotLoaded, 1);

        assert_eq!(decision, BootstrapDecision::Fail);
    }

    #[test]
    fn decide_after_bootstrap_fails_an_input_output_error_on_the_last_attempt() {
        let decision: BootstrapDecision = decide_after_bootstrap(Some(5), &BootoutOutcome::Unloaded, MAX_BOOTSTRAP_ATTEMPTS);

        assert_eq!(decision, BootstrapDecision::Fail);
    }

    #[test]
    fn decide_after_bootstrap_fails_another_exit_code_after_unloading_a_job() {
        let decision: BootstrapDecision = decide_after_bootstrap(Some(1), &BootoutOutcome::Unloaded, 1);

        assert_eq!(decision, BootstrapDecision::Fail);
    }
}
