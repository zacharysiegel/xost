mod browser;
mod clock;
mod config;
mod cron;
mod error;
mod notify;
mod queue;
mod register;
mod schedule;
mod secrets;
mod thread;

use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

use crate::browser::PostFailure;
use crate::browser::Session;
use crate::config::Config;
use crate::error::AppError;
use crate::queue::QueuedTweet;
use crate::schedule::ScheduleAction;

const MAX_POST_ATTEMPTS: u32 = 3;

#[derive(Parser)]
#[command(name = "xost", version, about = "Posts queued tweets to an X account via a browser session.")]
struct Cli {
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,

    #[arg(long, value_name = "NAME", global = true, default_value = "default")]
    account: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Register {
        #[arg(value_name = "ACCOUNT")]
        account_name: String,
    },
    #[command(flatten)]
    Account(AccountCommand),
}

#[derive(Subcommand)]
enum AccountCommand {
    Post,
    Login,
    AutoLogin,
    Diagnose,
    ExportSession,
    ImportSession {
        #[arg(long)]
        from: PathBuf,
    },
    Enqueue {
        #[arg(long)]
        heading: Option<String>,

        #[arg(long)]
        body: String,

        #[arg(long, value_name = "URL")]
        source_url: Option<String>,

        #[arg(long)]
        order: Option<i64>,
    },
    Schedule {
        #[arg(long, conflicts_with = "print")]
        remove: bool,

        #[arg(long)]
        print: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xost: {}", error);
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), AppError> {
    let cli: Cli = Cli::parse();

    let config_path: PathBuf = match &cli.config {
        Some(path) => path.clone(),
        None => config::default_config_path()?,
    };

    match cli.command {
        Command::Register { account_name } => register::run(&config_path, cli.config.as_deref(), &account_name),
        Command::Account(account_command) => run_account_command(account_command, &config_path, &cli.account).await,
    }
}

async fn run_account_command(account_command: AccountCommand, config_path: &Path, account: &str) -> Result<(), AppError> {
    match account_command {
        AccountCommand::Schedule { remove: true, .. } => remove_schedule(account),
        configured_account_command => run_configured_account_command(configured_account_command, config_path, account).await,
    }
}

fn remove_schedule(account: &str) -> Result<(), AppError> {
    config::validate_account_name(account)?;

    schedule::remove(account)
}

async fn run_configured_account_command(
    account_command: AccountCommand,
    config_path: &Path,
    account: &str,
) -> Result<(), AppError> {
    let config: Config = Config::load(config_path, account)?;

    match account_command {
        AccountCommand::Post => run_post(&config).await,
        AccountCommand::Login => run_login(&config).await,
        AccountCommand::AutoLogin => run_auto_login(&config).await,
        AccountCommand::Diagnose => run_diagnose(&config).await,
        AccountCommand::ExportSession => run_export_session(&config).await,
        AccountCommand::ImportSession { from } => run_import_session(&config, &from).await,
        AccountCommand::Enqueue {
            heading,
            body,
            source_url,
            order,
        } => run_enqueue(&config, heading.as_deref(), &body, source_url.as_deref(), order),
        // Removal is dispatched before the config loads, in run_account_command.
        AccountCommand::Schedule { remove: _, print } => schedule::run(&config, config_path, schedule_action(print)),
    }
}

fn schedule_action(print: bool) -> ScheduleAction {
    match print {
        true => ScheduleAction::Print,
        false => ScheduleAction::Install,
    }
}

async fn run_export_session(config: &Config) -> Result<(), AppError> {
    let session: Session = Session::launch(config).await?;

    let export_result: Result<String, AppError> = session.export_cookies().await;

    session.close().await.ok();

    let cookies_json: String = export_result?;
    println!("{}", cookies_json);

    Ok(())
}

async fn run_import_session(config: &Config, from: &std::path::Path) -> Result<(), AppError> {
    let cookies_json: String = std::fs::read_to_string(from)?;

    let session: Session = Session::launch(config).await?;

    let import_result: Result<(), AppError> = session.import_cookies(&cookies_json).await;

    session.close().await.ok();

    import_result?;
    println!("session imported; this profile is now logged in");

    Ok(())
}

async fn run_auto_login(config: &Config) -> Result<(), AppError> {
    let mut test_profile: std::ffi::OsString = config.profile_dir.clone().into_os_string();
    test_profile.push("-logintest");
    let test_profile: PathBuf = PathBuf::from(test_profile);

    let session: Session = Session::launch_with_profile(config, &test_profile).await?;

    let result: Result<(), AppError> = session.attempt_login(config, "/tmp/xost-login-test.png").await;

    session.close().await.ok();

    match result {
        Ok(()) => {
            println!("automated login succeeded; manual login is not required for this account");
            Ok(())
        }
        Err(error) => {
            println!("automated login failed: {}", error);
            println!("see /tmp/xost-login-test.png for where it stopped");
            Err(error)
        }
    }
}

async fn run_diagnose(config: &Config) -> Result<(), AppError> {
    let session: Session = Session::launch(config).await?;

    let outcome: Result<(), AppError> = session.diagnose("/tmp/xost-diag.png").await;

    session.close().await.ok();

    outcome
}

fn run_enqueue(
    config: &Config,
    heading: Option<&str>,
    body: &str,
    source_url: Option<&str>,
    order: Option<i64>,
) -> Result<(), AppError> {
    let connection: rusqlite::Connection = queue::open(&config.db_path)?;
    queue::init_schema(&connection)?;

    let id: i64 = queue::enqueue(&connection, heading, body, source_url, order)?;
    println!("enqueued tweet id {}", id);

    Ok(())
}

async fn run_login(config: &Config) -> Result<(), AppError> {
    browser::run_manual_login(config)?;

    println!("browser closed; if you signed in, the session is saved for `post`");

    Ok(())
}

async fn run_post(config: &Config) -> Result<(), AppError> {
    let connection: rusqlite::Connection = queue::open(&config.db_path)?;
    queue::init_schema(&connection)?;

    let stuck_tweet_lookup: Option<QueuedTweet> = queue::find_stuck(&connection)?;

    if let Some(stuck_tweet) = stuck_tweet_lookup {
        let message: String = format!(
            "tweet id {} is stuck mid-post from a previous run; not retrying to avoid a duplicate. \
             Confirm on X whether it posted, then clear its posting_started_at.",
            stuck_tweet.id,
        );
        notify::send(&notification_title(config, "manual check needed"), &message);

        return Err(AppError::new(&message));
    }

    let next_tweet_lookup: Option<QueuedTweet> = queue::select_next(&connection)?;

    let next_tweet: QueuedTweet = match next_tweet_lookup {
        Some(tweet) => tweet,
        None => {
            println!("queue has no enabled, unfailed tweets; nothing to post");
            return Ok(());
        }
    };

    let post_texts: Vec<String> = split_into_thread_or_mark_failed(config, &connection, &next_tweet)?;

    post_with_retries(config, &connection, &next_tweet, &post_texts).await
}

async fn post_with_retries(
    config: &Config,
    connection: &rusqlite::Connection,
    tweet: &QueuedTweet,
    post_texts: &[String],
) -> Result<(), AppError> {
    let mut attempt_number: u32 = 1;

    loop {
        let outcome: AttemptOutcome = attempt_post(config, connection, tweet, post_texts).await?;

        let (failure_kind, error): (PrePublishFailureKind, AppError) = match outcome {
            AttemptOutcome::Posted | AttemptOutcome::SkippedAlreadyPostedToday => return Ok(()),
            AttemptOutcome::FailedBeforePublish { failure_kind, error } => (failure_kind, error),
        };

        let error_message: String = error.to_string();
        let retry_decision: RetryDecision = decide_after_failed_attempt(attempt_number, failure_kind);

        match retry_decision {
            RetryDecision::RetryImmediately => {
                if failure_kind == PrePublishFailureKind::Record {
                    queue::record_failed_attempt(connection, tweet.id, &error_message)?;
                }

                println!(
                    "attempt {} of {} for tweet id {} failed {}; retrying: {}",
                    attempt_number,
                    MAX_POST_ATTEMPTS,
                    tweet.id,
                    failure_kind.description(),
                    error_message,
                );

                attempt_number += 1;
            }
            RetryDecision::MarkRecordFailed => {
                let failure_summary: String = format!(
                    "failed before publishing on the last of {} posting attempts: {}",
                    attempt_number, error_message,
                );
                let message: String = mark_failed_and_notify(config, connection, tweet.id, &error_message, &failure_summary)?;

                return Err(AppError::new(&message));
            }
            RetryDecision::GiveUpLeavingRecordQueued => {
                let message: String = notify_environment_problem(config, tweet.id, &error_message);

                return Err(AppError::new(&message));
            }
        }
    }
}

enum AttemptOutcome {
    Posted,
    SkippedAlreadyPostedToday,
    FailedBeforePublish {
        failure_kind: PrePublishFailureKind,
        error: AppError,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum PrePublishFailureKind {
    Environment,
    Record,
}

impl PrePublishFailureKind {
    fn description(self) -> &'static str {
        match self {
            PrePublishFailureKind::Environment => "before X's composer was ready",
            PrePublishFailureKind::Record => "before publishing",
        }
    }
}

#[derive(PartialEq, Debug)]
enum RetryDecision {
    RetryImmediately,
    MarkRecordFailed,
    GiveUpLeavingRecordQueued,
}

fn decide_after_failed_attempt(attempt_number: u32, failure_kind: PrePublishFailureKind) -> RetryDecision {
    if attempt_number < MAX_POST_ATTEMPTS {
        return RetryDecision::RetryImmediately;
    }

    match failure_kind {
        PrePublishFailureKind::Environment => RetryDecision::GiveUpLeavingRecordQueued,
        PrePublishFailureKind::Record => RetryDecision::MarkRecordFailed,
    }
}

async fn attempt_post(
    config: &Config,
    connection: &rusqlite::Connection,
    tweet: &QueuedTweet,
    post_texts: &[String],
) -> Result<AttemptOutcome, AppError> {
    let launch_result: Result<Session, AppError> = Session::launch(config).await;

    let session: Session = match launch_result {
        Ok(session) => session,
        Err(error) => {
            return Ok(AttemptOutcome::FailedBeforePublish {
                failure_kind: PrePublishFailureKind::Environment,
                error,
            });
        }
    };

    let outcome: Result<AttemptOutcome, AppError> = post_in_session(&session, config, connection, tweet, post_texts).await;

    session.close().await.ok();

    outcome
}

async fn post_in_session(
    session: &Session,
    config: &Config,
    connection: &rusqlite::Connection,
    tweet: &QueuedTweet,
    post_texts: &[String],
) -> Result<AttemptOutcome, AppError> {
    let login_result: Result<(), AppError> = session.ensure_logged_in(config).await;

    if let Err(error) = login_result {
        notify::send(
            &notification_title(config, "sign-in needed"),
            &format!(
                "Could not establish a logged-in session. Run `xost --account {} login` to refresh it.",
                config.account,
            ),
        );

        return Err(error);
    }

    let already_posted_today: bool = session.already_posted_today(config).await?;

    if already_posted_today {
        println!("a post already exists on the account today; skipping");

        return Ok(AttemptOutcome::SkippedAlreadyPostedToday);
    }

    queue::mark_posting(connection, tweet.id)?;

    let post_result: Result<(), PostFailure> = session.post(post_texts).await;

    match post_result {
        Ok(()) => {
            queue::mark_posted(connection, tweet.id)?;
            println!("posted tweet id {}", tweet.id);

            Ok(AttemptOutcome::Posted)
        }
        Err(PostFailure::EnvironmentBeforeCompose(error)) => {
            queue::release_lock(connection, tweet.id)?;

            Ok(AttemptOutcome::FailedBeforePublish {
                failure_kind: PrePublishFailureKind::Environment,
                error,
            })
        }
        Err(PostFailure::RecordBeforePublish(error)) => Ok(AttemptOutcome::FailedBeforePublish {
            failure_kind: PrePublishFailureKind::Record,
            error,
        }),
        Err(PostFailure::AfterPublish(error)) => {
            let message: String = format!(
                "publish was attempted but not confirmed for tweet id {}; leaving it locked for manual \
                 review to avoid a duplicate: {}",
                tweet.id, error,
            );
            notify::send(&notification_title(config, "manual check needed"), &message);

            Err(AppError::new(&message))
        }
    }
}

fn split_into_thread_or_mark_failed(
    config: &Config,
    connection: &rusqlite::Connection,
    tweet: &QueuedTweet,
) -> Result<Vec<String>, AppError> {
    let post_text: String = compose_post_text(tweet, &config.heading_separator);
    let split_result: Result<Vec<String>, AppError> =
        thread::split_post_text(&post_text, tweet.source_url.as_deref(), &config.thread_settings);

    match split_result {
        Ok(post_texts) => Ok(post_texts),
        Err(error) => {
            let error_message: String = error.to_string();
            let failure_summary: String = format!("could not be split into a thread: {}", error_message);
            let message: String = mark_failed_and_notify(config, connection, tweet.id, &error_message, &failure_summary)?;

            Err(AppError::new(&message))
        }
    }
}

fn mark_failed_and_notify(
    config: &Config,
    connection: &rusqlite::Connection,
    tweet_id: i64,
    last_error: &str,
    failure_summary: &str,
) -> Result<String, AppError> {
    queue::mark_failed(connection, tweet_id, last_error)?;

    let message: String = format!(
        "tweet id {} {}; it is skipped on future runs. To requeue it: \
         update tweet_queue set failed_at = null where id = {};",
        tweet_id, failure_summary, tweet_id,
    );
    notify::send(&notification_title(config, "post failed; record skipped"), &message);

    Ok(message)
}

fn notify_environment_problem(config: &Config, tweet_id: i64, last_error: &str) -> String {
    let message: String = format!(
        "the last posting attempt failed before X's composer was ready: {}. Tweet id {} was not marked failed; \
         the next run retries it.",
        last_error, tweet_id,
    );
    notify::send(&notification_title(config, "posting environment problem"), &message);

    message
}

fn notification_title(config: &Config, summary: &str) -> String {
    format!("xost [{}]: {}", config.account, summary)
}

fn compose_post_text(tweet: &QueuedTweet, heading_separator: &str) -> String {
    match &tweet.heading {
        Some(heading) => format!("{}{}{}", heading, heading_separator, tweet.body),
        None => tweet.body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_post_text_joins_heading_and_body_with_the_separator() {
        let tweet: QueuedTweet = QueuedTweet {
            id: 1,
            heading: Some("1.".to_string()),
            body: "Text".to_string(),
            source_url: None,
        };

        assert_eq!(compose_post_text(&tweet, "\n\n"), "1.\n\nText");
    }

    #[test]
    fn compose_post_text_is_the_body_alone_without_a_heading() {
        let tweet: QueuedTweet = QueuedTweet {
            id: 1,
            heading: None,
            body: "Text".to_string(),
            source_url: None,
        };

        assert_eq!(compose_post_text(&tweet, "\n\n"), "Text");
    }

    #[test]
    fn decide_after_failed_attempt_retries_either_failure_kind_before_the_last_attempt() {
        let environment_decision: RetryDecision =
            decide_after_failed_attempt(MAX_POST_ATTEMPTS - 1, PrePublishFailureKind::Environment);
        let record_decision: RetryDecision = decide_after_failed_attempt(MAX_POST_ATTEMPTS - 1, PrePublishFailureKind::Record);

        assert_eq!(environment_decision, RetryDecision::RetryImmediately);
        assert_eq!(record_decision, RetryDecision::RetryImmediately);
    }

    #[test]
    fn decide_after_failed_attempt_marks_the_record_failed_after_a_record_failure_on_the_last_attempt() {
        let decision: RetryDecision = decide_after_failed_attempt(MAX_POST_ATTEMPTS, PrePublishFailureKind::Record);

        assert_eq!(decision, RetryDecision::MarkRecordFailed);
    }

    #[test]
    fn decide_after_failed_attempt_leaves_the_record_queued_after_an_environment_failure_on_the_last_attempt() {
        let decision: RetryDecision = decide_after_failed_attempt(MAX_POST_ATTEMPTS, PrePublishFailureKind::Environment);

        assert_eq!(decision, RetryDecision::GiveUpLeavingRecordQueued);
    }
}
