use rusqlite::Connection;
use rusqlite::params;

use crate::clock;
use crate::error::AppError;

#[derive(Clone, Debug, PartialEq)]
pub struct AlertRecord {
    pub condition_key: String,
    pub alerted_value: String,
}

pub fn create_table_if_missing(connection: &Connection) -> Result<(), AppError> {
    connection.execute_batch(
        "create table if not exists alert_state (
             condition_key text primary key,
             alerted_value text not null,
             alerted_at text not null
         );",
    )?;

    Ok(())
}

pub fn read_alert_records(connection: &Connection) -> Result<Vec<AlertRecord>, AppError> {
    let mut statement: rusqlite::Statement<'_> =
        connection.prepare("select condition_key, alerted_value from alert_state order by condition_key asc")?;

    let alert_records: Vec<AlertRecord> = statement
        .query_map([], |record| {
            Ok(AlertRecord {
                condition_key: record.get(0)?,
                alerted_value: record.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<AlertRecord>>>()?;

    Ok(alert_records)
}

pub fn record_alert(connection: &Connection, condition_key: &str, alerted_value: &str) -> Result<(), AppError> {
    let now: String = clock::now_rfc3339();

    connection.execute(
        "insert into alert_state (condition_key, alerted_value, alerted_at) values (?1, ?2, ?3)
         on conflict (condition_key) do update set alerted_value = excluded.alerted_value, alerted_at = excluded.alerted_at",
        params![condition_key, alerted_value, now],
    )?;

    Ok(())
}

pub fn clear_alert(connection: &Connection, condition_key: &str) -> Result<(), AppError> {
    connection.execute("delete from alert_state where condition_key = ?1", params![condition_key])?;

    Ok(())
}
