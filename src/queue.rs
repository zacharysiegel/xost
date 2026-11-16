use std::path::Path;

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::alert_state;
use crate::clock;
use crate::error::AppError;

const COLUMNS: &str = "id, heading, body, source_url";
const TWEET_QUEUE_COLUMNS: [TweetQueueColumn; 12] = [
    TweetQueueColumn::created_with_table("id", "integer primary key autoincrement"),
    TweetQueueColumn::added_when_missing("heading", "text"),
    TweetQueueColumn::created_with_table("body", "text not null"),
    TweetQueueColumn::created_with_table("queue_order", "integer not null"),
    TweetQueueColumn::created_with_table("enabled", "integer not null default 1"),
    TweetQueueColumn::created_with_table("last_posted_at", "text"),
    TweetQueueColumn::created_with_table("posting_started_at", "text"),
    TweetQueueColumn::created_with_table("attempts", "integer not null default 0"),
    TweetQueueColumn::created_with_table("last_error", "text"),
    TweetQueueColumn::created_with_table("created_at", "text not null default (strftime('%Y-%m-%dT%H:%M:%SZ','now'))"),
    TweetQueueColumn::added_when_missing("source_url", "text"),
    TweetQueueColumn::added_when_missing("failed_at", "text"),
];

pub struct QueuedTweet {
    pub id: i64,
    pub heading: Option<String>,
    pub body: String,
    pub source_url: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct StuckRecord {
    pub id: i64,
    pub heading: Option<String>,
    pub posting_started_at: String,
}

enum ColumnMigration {
    CreatedWithTable,
    AddedWhenMissing,
}

struct TweetQueueColumn {
    name: &'static str,
    definition: &'static str,
    migration: ColumnMigration,
}

impl TweetQueueColumn {
    const fn created_with_table(name: &'static str, definition: &'static str) -> TweetQueueColumn {
        TweetQueueColumn {
            name,
            definition,
            migration: ColumnMigration::CreatedWithTable,
        }
    }

    const fn added_when_missing(name: &'static str, definition: &'static str) -> TweetQueueColumn {
        TweetQueueColumn {
            name,
            definition,
            migration: ColumnMigration::AddedWhenMissing,
        }
    }
}

pub fn open(path: &Path) -> Result<Connection, AppError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let connection: Connection = Connection::open(path)?;

    Ok(connection)
}

pub fn init_schema(connection: &Connection) -> Result<(), AppError> {
    let column_definitions: Vec<String> = TWEET_QUEUE_COLUMNS
        .iter()
        .map(|column| format!("{} {}", column.name, column.definition))
        .collect();

    connection.execute_batch(&format!("create table if not exists tweet_queue ({});", column_definitions.join(", ")))?;

    rename_overflow_url_to_source_url_if_present(connection)?;
    add_missing_columns(connection)?;
    alert_state::create_table_if_missing(connection)?;

    Ok(())
}

fn add_missing_columns(connection: &Connection) -> Result<(), AppError> {
    let missing_column_names: Vec<String> = read_missing_columns(connection)?;

    for column in &TWEET_QUEUE_COLUMNS {
        let column_missing: bool = missing_column_names.iter().any(|missing_column_name| missing_column_name == column.name);

        if column_missing && matches!(column.migration, ColumnMigration::AddedWhenMissing) {
            connection.execute_batch(&format!("alter table tweet_queue add column {} {};", column.name, column.definition))?;
        }
    }

    Ok(())
}

fn rename_overflow_url_to_source_url_if_present(connection: &Connection) -> Result<(), AppError> {
    let overflow_url_present: bool = column_exists(connection, "overflow_url")?;
    let source_url_present: bool = column_exists(connection, "source_url")?;

    if overflow_url_present && !source_url_present {
        connection.execute_batch("alter table tweet_queue rename column overflow_url to source_url;")?;
    }

    Ok(())
}

pub fn read_missing_columns(connection: &Connection) -> Result<Vec<String>, AppError> {
    let mut missing_column_names: Vec<String> = Vec::new();

    for column in &TWEET_QUEUE_COLUMNS {
        let column_present: bool = column_exists(connection, column.name)?;

        if !column_present {
            missing_column_names.push(column.name.to_string());
        }
    }

    Ok(missing_column_names)
}

fn column_exists(connection: &Connection, column_name: &str) -> Result<bool, AppError> {
    let column_count: i64 = connection.query_row(
        "select count(*) from pragma_table_info('tweet_queue') where name = ?1",
        params![column_name],
        |row| row.get(0),
    )?;

    Ok(column_count > 0)
}

pub fn read_stuck_records(connection: &Connection) -> Result<Vec<StuckRecord>, AppError> {
    let mut statement: rusqlite::Statement<'_> = connection.prepare(
        "select id, heading, posting_started_at from tweet_queue where posting_started_at is not null order by id asc",
    )?;

    let stuck_records: Vec<StuckRecord> = statement
        .query_map([], |record| {
            Ok(StuckRecord {
                id: record.get(0)?,
                heading: record.get(1)?,
                posting_started_at: record.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<StuckRecord>>>()?;

    Ok(stuck_records)
}

pub fn find_stuck(connection: &Connection) -> Result<Option<StuckRecord>, AppError> {
    let stuck_records: Vec<StuckRecord> = read_stuck_records(connection)?;

    Ok(stuck_records.into_iter().next())
}

pub fn select_next(connection: &Connection) -> Result<Option<QueuedTweet>, AppError> {
    let query: String = format!(
        "select {COLUMNS} from tweet_queue
         where enabled = 1 and posting_started_at is null and failed_at is null
         order by last_posted_at asc nulls first, queue_order asc
         limit 1"
    );

    let next_tweet: Option<QueuedTweet> = connection
        .query_row(&query, [], map_row)
        .optional()?;

    Ok(next_tweet)
}

pub fn mark_posting(connection: &Connection, id: i64) -> Result<(), AppError> {
    let now: String = clock::now_rfc3339();

    connection.execute(
        "update tweet_queue set posting_started_at = ?1 where id = ?2",
        params![now, id],
    )?;

    Ok(())
}

pub fn mark_posted(connection: &Connection, id: i64) -> Result<(), AppError> {
    let now: String = clock::now_rfc3339();

    connection.execute(
        "update tweet_queue
         set last_posted_at = ?1, posting_started_at = null, attempts = 0, last_error = null
         where id = ?2",
        params![now, id],
    )?;

    Ok(())
}

pub fn release_lock(connection: &Connection, id: i64) -> Result<(), AppError> {
    connection.execute("update tweet_queue set posting_started_at = null where id = ?1", params![id])?;

    Ok(())
}

pub fn record_failed_attempt(connection: &Connection, id: i64, error_message: &str) -> Result<(), AppError> {
    connection.execute(
        "update tweet_queue
         set posting_started_at = null, attempts = attempts + 1, last_error = ?1
         where id = ?2",
        params![error_message, id],
    )?;

    Ok(())
}

pub fn mark_failed(connection: &Connection, id: i64, error_message: &str) -> Result<(), AppError> {
    let now: String = clock::now_rfc3339();

    connection.execute(
        "update tweet_queue
         set failed_at = ?1, posting_started_at = null, attempts = attempts + 1, last_error = ?2
         where id = ?3",
        params![now, error_message, id],
    )?;

    Ok(())
}

pub fn enqueue(
    connection: &Connection,
    heading: Option<&str>,
    body: &str,
    source_url: Option<&str>,
    queue_order: Option<i64>,
) -> Result<i64, AppError> {
    let resolved_order: i64 = match queue_order {
        Some(order) => order,
        None => next_order(connection)?,
    };

    connection.execute(
        "insert into tweet_queue (heading, body, source_url, queue_order) values (?1, ?2, ?3, ?4)",
        params![heading, body, source_url, resolved_order],
    )?;

    Ok(connection.last_insert_rowid())
}

fn next_order(connection: &Connection) -> Result<i64, AppError> {
    let order: i64 = connection.query_row(
        "select coalesce(max(queue_order), 0) + 1 from tweet_queue",
        [],
        |row| row.get(0),
    )?;

    Ok(order)
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedTweet> {
    Ok(QueuedTweet {
        id: row.get("id")?,
        heading: row.get("heading")?,
        body: row.get("body")?,
        source_url: row.get("source_url")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_fresh_db() -> Connection {
        let connection: Connection = Connection::open_in_memory().unwrap();
        init_schema(&connection).unwrap();

        connection
    }

    fn get_last_posted_at(connection: &Connection, id: i64) -> Option<String> {
        connection
            .query_row("select last_posted_at from tweet_queue where id = ?1", params![id], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn enqueue_assigns_incrementing_order() {
        let connection: Connection = create_fresh_db();

        enqueue(&connection, None, "a", None, None).unwrap();
        enqueue(&connection, None, "b", None, None).unwrap();

        let first_order: i64 = connection
            .query_row("select queue_order from tweet_queue where body = 'a'", [], |row| row.get(0))
            .unwrap();
        let second_order: i64 = connection
            .query_row("select queue_order from tweet_queue where body = 'b'", [], |row| row.get(0))
            .unwrap();

        assert_eq!(first_order, 1);
        assert_eq!(second_order, 2);
    }

    #[test]
    fn init_schema_adds_missing_columns_to_a_table_created_without_them() {
        let connection: Connection = Connection::open_in_memory().unwrap();

        connection
            .execute_batch("create table tweet_queue (id integer primary key autoincrement, body text not null, queue_order integer not null);")
            .unwrap();

        init_schema(&connection).unwrap();

        let added_column_count: i64 = connection
            .query_row(
                "select count(*) from pragma_table_info('tweet_queue') where name in ('heading', 'source_url', 'failed_at')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(added_column_count, 3);
    }

    #[test]
    fn init_schema_adds_the_alert_state_table_to_an_existing_database() {
        let connection: Connection = Connection::open_in_memory().unwrap();

        connection
            .execute_batch("create table tweet_queue (id integer primary key autoincrement, body text not null, queue_order integer not null);")
            .unwrap();

        init_schema(&connection).unwrap();

        let alert_state_column_names: Vec<String> = connection
            .prepare("select name from pragma_table_info('alert_state') order by cid asc")
            .unwrap()
            .query_map([], |record| record.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();

        assert_eq!(alert_state_column_names, vec!["condition_key", "alerted_value", "alerted_at"]);
    }

    #[test]
    fn init_schema_renames_overflow_url_to_source_url_keeping_its_values() {
        let connection: Connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "create table tweet_queue (id integer primary key autoincrement, body text not null, queue_order integer not null, overflow_url text);
                 insert into tweet_queue (body, queue_order, overflow_url) values ('a', 1, 'https://example.com/full-text');",
            )
            .unwrap();

        init_schema(&connection).unwrap();

        let source_url: Option<String> = connection
            .query_row("select source_url from tweet_queue where body = 'a'", [], |row| row.get(0))
            .unwrap();

        assert_eq!(source_url.as_deref(), Some("https://example.com/full-text"));
        assert!(!column_exists(&connection, "overflow_url").unwrap());
    }

    #[test]
    fn select_next_returns_the_source_url_given_to_enqueue() {
        let connection: Connection = create_fresh_db();
        enqueue(&connection, None, "a", Some("https://example.com/full-text"), None).unwrap();

        let next: QueuedTweet = select_next(&connection).unwrap().unwrap();

        assert_eq!(next.source_url.as_deref(), Some("https://example.com/full-text"));
    }

    #[test]
    fn select_next_is_none_when_queue_empty() {
        let connection: Connection = create_fresh_db();

        assert!(select_next(&connection).unwrap().is_none());
    }

    #[test]
    fn select_next_returns_lowest_queue_order_when_none_posted() {
        let connection: Connection = create_fresh_db();
        enqueue(&connection, None, "later", None, Some(2)).unwrap();
        enqueue(&connection, None, "sooner", None, Some(1)).unwrap();

        let next: QueuedTweet = select_next(&connection).unwrap().unwrap();

        assert_eq!(next.body, "sooner");
    }

    #[test]
    fn select_next_rotates_back_to_start_after_all_posted() {
        let connection: Connection = create_fresh_db();
        enqueue(&connection, None, "a", None, Some(1)).unwrap();
        enqueue(&connection, None, "b", None, Some(2)).unwrap();

        let first: QueuedTweet = select_next(&connection).unwrap().unwrap();
        mark_posted(&connection, first.id).unwrap();

        let second: QueuedTweet = select_next(&connection).unwrap().unwrap();
        mark_posted(&connection, second.id).unwrap();

        let wrapped: QueuedTweet = select_next(&connection).unwrap().unwrap();

        assert_eq!(first.body, "a");
        assert_eq!(second.body, "b");
        assert_eq!(wrapped.body, "a");
    }

    #[test]
    fn select_next_skips_a_locked_row() {
        let connection: Connection = create_fresh_db();
        enqueue(&connection, None, "a", None, Some(1)).unwrap();
        enqueue(&connection, None, "b", None, Some(2)).unwrap();

        let first: QueuedTweet = select_next(&connection).unwrap().unwrap();
        mark_posting(&connection, first.id).unwrap();

        let next: QueuedTweet = select_next(&connection).unwrap().unwrap();

        assert_eq!(first.body, "a");
        assert_eq!(next.body, "b");
    }

    #[test]
    fn select_next_skips_a_failed_record() {
        let connection: Connection = create_fresh_db();
        let failed_id: i64 = enqueue(&connection, None, "a", None, Some(1)).unwrap();
        enqueue(&connection, None, "b", None, Some(2)).unwrap();
        connection
            .execute(
                "update tweet_queue set failed_at = '2026-01-01T00:00:00Z' where id = ?1",
                params![failed_id],
            )
            .unwrap();

        let next: QueuedTweet = select_next(&connection).unwrap().unwrap();

        assert_eq!(next.body, "b");
    }

    #[test]
    fn find_stuck_reports_only_a_locked_row() {
        let connection: Connection = create_fresh_db();
        let id: i64 = enqueue(&connection, None, "a", None, None).unwrap();

        assert!(find_stuck(&connection).unwrap().is_none());

        mark_posting(&connection, id).unwrap();

        assert_eq!(find_stuck(&connection).unwrap().unwrap().id, id);
    }

    #[test]
    fn read_stuck_records_returns_every_locked_record_with_its_lock_time() {
        let connection: Connection = create_fresh_db();
        let first_stuck_id: i64 = enqueue(&connection, Some("1."), "a", None, None).unwrap();
        enqueue(&connection, None, "b", None, None).unwrap();
        let second_stuck_id: i64 = enqueue(&connection, None, "c", None, None).unwrap();
        connection
            .execute(
                "update tweet_queue set posting_started_at = '2026-11-15T09:00:00Z' where id in (?1, ?2)",
                params![first_stuck_id, second_stuck_id],
            )
            .unwrap();

        let stuck_records: Vec<StuckRecord> = read_stuck_records(&connection).unwrap();

        let expected_stuck_records: Vec<StuckRecord> = vec![
            StuckRecord {
                id: first_stuck_id,
                heading: Some("1.".to_string()),
                posting_started_at: "2026-11-15T09:00:00Z".to_string(),
            },
            StuckRecord {
                id: second_stuck_id,
                heading: None,
                posting_started_at: "2026-11-15T09:00:00Z".to_string(),
            },
        ];
        assert_eq!(stuck_records, expected_stuck_records);
    }

    #[test]
    fn record_failed_attempt_unlocks_and_keeps_the_record_selectable() {
        let connection: Connection = create_fresh_db();
        let id: i64 = enqueue(&connection, None, "a", None, None).unwrap();
        mark_posting(&connection, id).unwrap();

        record_failed_attempt(&connection, id, "boom").unwrap();

        assert!(find_stuck(&connection).unwrap().is_none());
        assert_eq!(select_next(&connection).unwrap().unwrap().id, id);
    }

    #[test]
    fn release_lock_unlocks_without_recording_a_failure() {
        let connection: Connection = create_fresh_db();
        let id: i64 = enqueue(&connection, None, "a", None, None).unwrap();
        mark_posting(&connection, id).unwrap();

        release_lock(&connection, id).unwrap();

        let (attempts, last_error, failed_at): (i64, Option<String>, Option<String>) = connection
            .query_row(
                "select attempts, last_error, failed_at from tweet_queue where id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert!(find_stuck(&connection).unwrap().is_none());
        assert_eq!(attempts, 0);
        assert!(last_error.is_none());
        assert!(failed_at.is_none());
    }

    #[test]
    fn mark_failed_records_the_failure_time_and_error_and_clears_the_lock() {
        let connection: Connection = create_fresh_db();
        let id: i64 = enqueue(&connection, None, "a", None, None).unwrap();
        mark_posting(&connection, id).unwrap();

        mark_failed(&connection, id, "boom").unwrap();

        let (failed_at, last_error): (Option<String>, Option<String>) = connection
            .query_row(
                "select failed_at, last_error from tweet_queue where id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert!(find_stuck(&connection).unwrap().is_none());
        assert!(failed_at.is_some());
        assert_eq!(last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn mark_posted_clears_the_lock_and_records_the_time() {
        let connection: Connection = create_fresh_db();
        let id: i64 = enqueue(&connection, None, "a", None, None).unwrap();
        mark_posting(&connection, id).unwrap();

        mark_posted(&connection, id).unwrap();

        assert!(find_stuck(&connection).unwrap().is_none());
        assert!(get_last_posted_at(&connection, id).is_some());
    }
}
