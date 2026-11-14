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
use crate::error::AppError;

const LABEL_PREFIX: &str = "ro.zach.xost.";
const LAUNCH_DAEMONS_DIRECTORY: &str = "/Library/LaunchDaemons";
const SUDO_PATH: &str = "/usr/bin/sudo";
const LAUNCHCTL_PATH: &str = "/bin/launchctl";
const INSTALL_PATH: &str = "/usr/bin/install";
const RM_PATH: &str = "/bin/rm";
const ID_PATH: &str = "/usr/bin/id";
const ROOT_USER_NAME: &str = "root";
// launchctl bootout exits with ESRCH (3) or 113 ("Could not find specified service") when the job is not loaded.
const BOOTOUT_NOT_LOADED_EXIT_CODES: [i32; 2] = [3, 113];
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
    pub user_name: String,
    pub home_directory: String,
    pub executable_path: String,
    pub config_path: String,
    pub calendar_intervals: Vec<CalendarInterval>,
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

pub fn run(config: &Config, config_path: &Path, action: ScheduleAction) -> Result<(), AppError> {
    match action {
        ScheduleAction::Print => print_plist(config, config_path),
        ScheduleAction::Install => install(config, config_path),
    }
}

fn print_plist(config: &Config, config_path: &Path) -> Result<(), AppError> {
    let job: LaunchDaemonJob = create_launch_daemon_job(config, config_path)?;

    print!("{}", render_plist(&job));

    Ok(())
}

fn install(config: &Config, config_path: &Path) -> Result<(), AppError> {
    let job: LaunchDaemonJob = create_launch_daemon_job(config, config_path)?;
    let label: String = job_label(&config.account);
    let installed_plist_path: String = installed_plist_path(&label);

    let temporary_plist_path: PathBuf = std::env::temp_dir().join(format!("{}.{}.plist", label, std::process::id()));
    std::fs::write(&temporary_plist_path, render_plist(&job))?;

    let copy_result: Result<(), AppError> = copy_plist_into_launch_daemons(&temporary_plist_path, &installed_plist_path);
    std::fs::remove_file(&temporary_plist_path).ok();
    copy_result?;

    let bootout_outcome: BootoutOutcome = bootout(&label)?;
    bootstrap(&installed_plist_path, &bootout_outcome)?;

    println!("installed the LaunchDaemon");
    println!("Label: {}", label);
    println!("Plist: {}", installed_plist_path);
    println!("Log: {}", log_path(&job.home_directory, &job.account));

    Ok(())
}

pub fn remove(account: &str) -> Result<(), AppError> {
    let label: String = job_label(account);
    let installed_plist_path: String = installed_plist_path(&label);

    bootout(&label)?;
    delete_installed_plist(&installed_plist_path)?;

    println!("removed the LaunchDaemon");
    println!("Label: {}", label);
    println!("Plist: {}", installed_plist_path);

    Ok(())
}

fn create_launch_daemon_job(config: &Config, config_path: &Path) -> Result<LaunchDaemonJob, AppError> {
    let schedule: &CronSchedule = config.schedule.as_ref().ok_or_else(|| {
        AppError::new(&format!(
            "account '{}' has no {} key; add one to its config section, e.g. {} = 0 9 * * *",
            config.account,
            config::SCHEDULE_KEY,
            config::SCHEDULE_KEY,
        ))
    })?;

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

    Ok(LaunchDaemonJob {
        account: config.account.clone(),
        user_name,
        home_directory,
        executable_path: path_to_string(&executable_path)?,
        config_path: path_to_string(&absolute_config_path)?,
        calendar_intervals: calendar_intervals(schedule),
    })
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

    expand_days(with_months, &schedule.day_of_month, &schedule.day_of_week)
}

fn expand_days(intervals: Vec<CalendarInterval>, day_of_month: &CronField, day_of_week: &CronField) -> Vec<CalendarInterval> {
    let assign_day: fn(&mut CalendarInterval, u8) = |interval, value| interval.day = Some(value);
    let assign_weekday: fn(&mut CalendarInterval, u8) = |interval, value| interval.weekday = Some(value);

    match (day_of_month, day_of_week) {
        // Vixie cron fires when either day field matches only when neither field's text begins with '*'.
        (CronField::Only(_), CronField::Only(_)) => {
            let by_day_of_month: Vec<CalendarInterval> = expand(intervals.clone(), day_of_month, assign_day);
            let by_day_of_week: Vec<CalendarInterval> = expand(intervals, day_of_week, assign_weekday);

            by_day_of_month.into_iter().chain(by_day_of_week).collect()
        }
        _ => {
            let with_days: Vec<CalendarInterval> = expand(intervals, day_of_month, assign_day);

            expand(with_days, day_of_week, assign_weekday)
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
    let label: String = job_label(&job.account);
    let log_path: String = log_path(&job.home_directory, &job.account);
    let program_arguments: [&str; 6] = [&job.executable_path, "--config", &job.config_path, "--account", &job.account, "post"];

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

fn job_label(account: &str) -> String {
    format!("{}{}", LABEL_PREFIX, account)
}

fn installed_plist_path(label: &str) -> String {
    format!("{}/{}.plist", LAUNCH_DAEMONS_DIRECTORY, label)
}

fn log_path(home_directory: &str, account: &str) -> String {
    Path::new(home_directory)
        .join("Library")
        .join("Logs")
        .join(format!("xost-{}.log", account))
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

fn bootout(label: &str) -> Result<BootoutOutcome, AppError> {
    let service_target: String = format!("system/{}", label);

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

    fn intervals_for(expression: &str) -> Vec<CalendarInterval> {
        calendar_intervals(&CronSchedule::parse(expression).unwrap())
    }

    fn at(minute: Option<u8>, hour: Option<u8>, day: Option<u8>, weekday: Option<u8>) -> CalendarInterval {
        CalendarInterval {
            minute,
            hour,
            day,
            weekday,
            month: None,
        }
    }

    fn sample_job(account: &str) -> LaunchDaemonJob {
        LaunchDaemonJob {
            account: account.to_string(),
            user_name: "zachary".to_string(),
            home_directory: "/Users/zachary".to_string(),
            executable_path: "/opt/xost/bin/xost".to_string(),
            config_path: "/Users/zachary/.config/xost/config".to_string(),
            calendar_intervals: vec![at(Some(0), Some(9), None, None)],
        }
    }

    #[test]
    fn calendar_intervals_expands_a_minute_step() {
        let expected_intervals: Vec<CalendarInterval> = vec![
            at(Some(0), Some(9), None, None),
            at(Some(20), Some(9), None, None),
            at(Some(40), Some(9), None, None),
        ];

        assert_eq!(intervals_for("*/20 9 * * *"), expected_intervals);
    }

    #[test]
    fn calendar_intervals_omits_every_wildcard_field() {
        assert_eq!(intervals_for("* * * * *"), vec![CalendarInterval::unrestricted()]);
    }

    #[test]
    fn calendar_intervals_unions_day_of_month_and_day_of_week_when_both_are_restricted() {
        let expected_intervals: Vec<CalendarInterval> = vec![
            at(Some(0), Some(9), Some(1), None),
            at(Some(0), Some(9), Some(15), None),
            at(Some(0), Some(9), None, Some(0)),
        ];

        assert_eq!(intervals_for("0 9 1,15 * 7"), expected_intervals);
    }

    #[test]
    fn calendar_intervals_requires_both_day_fields_when_day_of_month_is_a_stepped_wildcard() {
        let expected_intervals: Vec<CalendarInterval> = (1..=31)
            .step_by(2)
            .map(|day| at(Some(0), Some(9), Some(day), Some(1)))
            .collect();

        assert_eq!(intervals_for("0 9 */2 * 1"), expected_intervals);
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

        assert_eq!(intervals_for("0 9 * 6,12 1"), expected_intervals);
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
        <string>/Users/zachary/.config/xost/config</string>
        <string>--account</string>
        <string>daily</string>
        <string>post</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>/Users/zachary</string>
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
    <string>/Users/zachary/Library/Logs/xost-daily.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/zachary/Library/Logs/xost-daily.log</string>
</dict>
</plist>
";

        assert_eq!(render_plist(&sample_job("daily")), expected_plist);
    }

    #[test]
    fn render_plist_escapes_xml_special_characters() {
        let mut job: LaunchDaemonJob = sample_job("daily");
        job.config_path = "/Users/zachary/<a & b> \"c\" 'd'".to_string();

        let plist: String = render_plist(&job);

        assert!(plist.contains("<string>/Users/zachary/&lt;a &amp; b&gt; &quot;c&quot; &apos;d&apos;</string>"));
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
