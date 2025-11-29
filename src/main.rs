use anyhow::{anyhow, Context};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use config::ConfigError;
use dotenv::dotenv;
use google_sheets4::{api::ValueRange, hyper_rustls, hyper_util, yup_oauth2, Sheets};
use serde::Deserialize;
use std::time::Duration as StdDuration;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, instrument, trace};

// --- Data Structures ---

/// Represents a Time Entry from Toggl's API (v9)
#[derive(Debug, Deserialize)]
struct TogglTimeEntry {
    #[allow(dead_code)]
    /// toggl time entry unique id
    id: i64,
    description: Option<String>,
    start: DateTime<Utc>,
    stop: Option<DateTime<Utc>>,
    duration: i64,
    project_id: Option<i64>,
    project_name: Option<String>,
    tags: Option<Vec<String>>,
}

// --- Toggl Logic ---
#[instrument]
async fn fetch_toggl_entries(
    api_token: &str,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
) -> anyhow::Result<Vec<TogglTimeEntry>> {
    let client = reqwest::Client::new();

    // Toggl v9 API Endpoint for "me/time_entries"
    let url = "https://api.track.toggl.com/api/v9/me/time_entries";

    info!("Fetching Toggl entries...");

    let response = client
        .get(url)
        .basic_auth(api_token, Some("api_token"))
        .query(&[
            ("start_date", start_date.to_rfc3339()),
            ("end_date", end_date.to_rfc3339()),
            // include meta entity data in the response (e.g. project_name etc.)
            ("meta", "true".to_string()),
        ])
        .send()
        .await?;

    if !response.status().is_success() {
        let error_text = response.text().await?;
        return Err(anyhow!("Toggl API Error: {error_text}"));
    }

    let entries: Vec<TogglTimeEntry> = response.json().await?;
    info!("Found {} entries.", entries.len());

    Ok(entries)
}

// --- Google Sheets Logic ---

// Helper to convert DateTime<Utc> to Google Sheets serial number (days since Dec 30, 1899)
fn to_serial_number(t: DateTime<Utc>) -> f64 {
    let epoch = NaiveDateTime::parse_from_str("1899-12-30 00:00:00", "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_utc();
    let duration = t.signed_duration_since(epoch);
    // Days + fraction of day
    #[allow(clippy::cast_precision_loss)]
    let serial_number = duration.num_milliseconds() as f64 / 86_400_000.0;
    serial_number
}

fn extract_time_from_row(row: &[serde_json::Value]) -> f64 {
    if let Some(val) = row.first() {
        if let Some(serial) = val.as_f64() {
            return serial;
        }
        if let Some(s) = val.as_str() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return to_serial_number(dt.with_timezone(&Utc));
            }
            if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S UTC") {
                return to_serial_number(ndt.and_utc());
            }
            if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
                return to_serial_number(ndt.and_utc());
            }
        }
    }
    0.0
}

const GOOGLE_SHEET_NAME: &str = "toggl_entries";

type GoogleAuthenticator = yup_oauth2::authenticator::Authenticator<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
>;

#[allow(clippy::too_many_lines)]
#[instrument(skip(new_entries, google_auth))]
async fn sync_sheet(
    spreadsheet_id: &str,
    new_entries: Vec<TogglTimeEntry>,
    cutoff_date: DateTime<Utc>,
    google_auth: GoogleAuthenticator,
) -> anyhow::Result<()> {
    debug!("building hyper client for google sheets requests");
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .build(),
        );
    let hub = Sheets::new(client, google_auth);

    // Check if the sheet exists
    let spreadsheet = hub.spreadsheets().get(spreadsheet_id).doit().await?.1;
    let sheet_exists = spreadsheet.sheets.as_ref().is_some_and(|sheets| {
        sheets.iter().any(|s| {
            s.properties
                .as_ref()
                .is_some_and(|p| p.title.as_deref() == Some(GOOGLE_SHEET_NAME))
        })
    });

    if !sheet_exists {
        info!("Sheet '{}' not found. Creating it...", GOOGLE_SHEET_NAME);
        let req = google_sheets4::api::BatchUpdateSpreadsheetRequest {
            requests: Some(vec![google_sheets4::api::Request {
                add_sheet: Some(google_sheets4::api::AddSheetRequest {
                    properties: Some(google_sheets4::api::SheetProperties {
                        title: Some(GOOGLE_SHEET_NAME.to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }]),
            include_spreadsheet_in_response: Some(false),
            response_ranges: None,
            response_include_grid_data: Some(false),
        };

        hub.spreadsheets()
            .batch_update(req, spreadsheet_id)
            .doit()
            .await?;
        info!("Sheet '{}' created.", GOOGLE_SHEET_NAME);
    }

    // 2. Read existing data
    // We use UNFORMATTED_VALUE to get numbers for dates if they are already in serial format
    let range = format!("'{GOOGLE_SHEET_NAME}'!A:F");
    let result = hub
        .spreadsheets()
        .values_get(spreadsheet_id, &range)
        .value_render_option("UNFORMATTED_VALUE")
        .doit()
        .await;

    let mut kept_rows: Vec<Vec<serde_json::Value>> = Vec::new();
    let header_row = vec![
        serde_json::json!("Start"),
        serde_json::json!("Description"),
        serde_json::json!("Duration (m)"),
        serde_json::json!("Project ID"),
        serde_json::json!("Project Name"),
        serde_json::json!("Tags"),
        serde_json::json!("Stop"),
    ];

    if let Ok((_, value_range)) = result {
        if let Some(rows) = value_range.values {
            info!("Read {} existing rows from Sheet.", rows.len());
            for row in rows {
                // Check if it's a header row
                if let Some(first_col) = row.first() {
                    if let Some(s) = first_col.as_str() {
                        if s == "Start" {
                            continue; // Skip header
                        }
                    }
                }

                // Try to parse the date from the first column (index 0)
                let should_keep = if let Some(date_val) = row.first() {
                    // Case A: It's a number (Serial Number)
                    if let Some(serial) = date_val.as_f64() {
                        let epoch = NaiveDateTime::parse_from_str(
                            "1899-12-30 00:00:00",
                            "%Y-%m-%d %H:%M:%S",
                        )
                        .unwrap()
                        .and_utc();
                        // Convert serial back to duration
                        #[allow(clippy::cast_possible_truncation)]
                        let millis = (serial * 86_400_000.0) as i64;
                        let dt = epoch + Duration::milliseconds(millis);
                        dt < cutoff_date
                    }
                    // Case B: It's a string (Legacy format)
                    else if let Some(date_str) = date_val.as_str() {
                        // Attempt 1: RFC3339 (standard)
                        if let Ok(dt) = DateTime::parse_from_rfc3339(date_str) {
                            dt.with_timezone(&Utc) < cutoff_date
                        }
                        // Attempt 2: "YYYY-MM-DD HH:MM:SS UTC" (common Display for Utc)
                        else if let Ok(ndt) =
                            NaiveDateTime::parse_from_str(date_str, "%Y-%m-%d %H:%M:%S UTC")
                        {
                            ndt.and_utc() < cutoff_date
                        }
                        // Attempt 3: Try without UTC suffix just in case
                        else if let Ok(ndt) =
                            NaiveDateTime::parse_from_str(date_str, "%Y-%m-%d %H:%M:%S")
                        {
                            ndt.and_utc() < cutoff_date
                        } else {
                            // Could not parse date string, keep the row (safe default)
                            true
                        }
                    } else {
                        true // Unknown type, keep it
                    }
                } else {
                    true // Empty row or no first col, keep it
                };

                if should_keep {
                    kept_rows.push(row);
                }
            }
        }
    }

    info!("Kept {} rows older than {}.", kept_rows.len(), cutoff_date);

    // 3. Convert new Toggl entries to Rows
    let mut new_rows: Vec<Vec<serde_json::Value>> = Vec::new();
    for entry in new_entries {
        #[allow(clippy::cast_precision_loss)]
        let duration_mins = entry.duration as f64 / 60.0;

        let stop_val = match entry.stop {
            Some(t) => serde_json::json!(to_serial_number(t)),
            None => serde_json::json!("Running"),
        };

        let tags_str = entry.tags.unwrap_or_default().join(", ");

        // Use Serial Number for start time
        let row = vec![
            serde_json::json!(to_serial_number(entry.start)),
            serde_json::json!(entry.description.unwrap_or_default()),
            serde_json::json!(duration_mins),
            serde_json::json!(entry
                .project_id
                .map_or_else(String::default, |id| id.to_string())),
            serde_json::json!(entry.project_name.unwrap_or_default()),
            serde_json::json!(tags_str),
            stop_val,
        ];
        new_rows.push(row);
    }

    debug!("new rows: {:?}", new_rows);

    info!("Adding {} new entries.", new_rows.len());

    // 4. Combine and Sort Rows
    let mut data_rows = kept_rows;
    data_rows.append(&mut new_rows);

    data_rows.sort_by(|a, b| {
        let time_a = extract_time_from_row(a);
        let time_b = extract_time_from_row(b);
        time_a
            .partial_cmp(&time_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut all_rows = vec![header_row];
    all_rows.append(&mut data_rows);

    // 5. Clear Sheet
    // We clear everything to ensure no stale data remains if the total row count decreases
    let clear_req = google_sheets4::api::ClearValuesRequest::default();
    hub.spreadsheets()
        .values_clear(
            clear_req,
            spreadsheet_id,
            &format!("'{GOOGLE_SHEET_NAME}'!A:F"),
        )
        .doit()
        .await?;

    // 6. Write All Data
    if all_rows.is_empty() {
        info!("No data to write.");
    } else {
        let req = ValueRange {
            values: Some(all_rows),
            ..Default::default()
        };

        hub.spreadsheets()
            .values_update(req, spreadsheet_id, &format!("'{GOOGLE_SHEET_NAME}'!A1"))
            .value_input_option("USER_ENTERED") // Important for Sheets to recognize numbers as potential dates
            .doit()
            .await?;
        info!("Successfully synced data to Google Sheet.");
    }

    Ok(())
}

// --- Main Execution ---
#[instrument(skip(toggl_token, spreadsheet_id, google_auth))]
async fn run_sync_task(
    toggl_token: &str,
    spreadsheet_id: &str,
    google_auth: GoogleAuthenticator,
) -> anyhow::Result<()> {
    // Define Time Range (Last 7 days)
    let end_date = Utc::now();
    let start_date = end_date - Duration::weeks(1);

    info!("Time Range: {} to {}", start_date, end_date);

    // Fetch from Toggl
    let entries = fetch_toggl_entries(toggl_token, start_date, end_date).await?;

    // Sync to Google Sheets
    // We use start_date as the cutoff: anything in the sheet >= start_date is replaced.
    sync_sheet(spreadsheet_id, entries, start_date, google_auth).await?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct Settings {
    toggl_api_token: String,
    #[serde(default = "default_svc_key_file")]
    google_service_account_key_file: String,
    google_sheets_spreadsheet_id: String,
    #[serde(default = "default_schedule_string")]
    sync_schedule: String,
}
fn default_svc_key_file() -> String {
    "google_private_key.json".to_string()
}
fn default_schedule_string() -> String {
    // Run every 30 minutes
    // Cron format: sec min hour day_of_month month day_of_week year
    // "0 */30 * * * *" means every 30th minute (0, 30)
    "0 */30 * * * *".to_string()
}
impl Settings {
    /// Get a new instance of Settings from the environment or config file.
    fn new() -> Result<Self, ConfigError> {
        let settings = config::Config::builder()
            // Add in `./settings.toml` (other extensions allowed as well)
            .add_source(config::File::with_name("settings").required(false))
            // Add in settings from the environment (with a prefix of TOGGL2SHEETS)
            // Eg.. `TOGGL2SHEETS_DEBUG=1 ./target/app` would set the `debug` key
            .add_source(config::Environment::with_prefix("TOGGL2SHEETS"))
            .build()
            .unwrap();

        settings.try_deserialize()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();

    // initialise tracing
    opentelemetry_tracing_utils::set_up_logging().expect("Tracing setup should work");

    // Get configuration settings
    let settings = Settings::new().context("settings should exist")?;
    info!("settings loaded");

    // Print out our settings. NOTE: Contains sensitive stuff.
    trace!("{settings:?}");

    let google_authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(
        yup_oauth2::read_service_account_key(settings.google_service_account_key_file)
            .await
            .expect("the google service account key file should exist"),
    )
    .persist_tokens_to_disk("google_tokencache.json")
    .build()
    .await
    .expect("yup authenticator should succeed");

    // Run immediately on startup
    if let Err(e) = run_sync_task(
        &settings.toggl_api_token,
        &settings.google_sheets_spreadsheet_id,
        google_authenticator.clone(),
    )
    .await
    {
        error!("Error during initial sync: {}", e);
    }

    let sched = JobScheduler::new().await?;

    info!(
        sync_schedule = settings.sync_schedule,
        "Setting up sync schedule"
    );

    sched
        .add(Job::new_async(
            settings.sync_schedule,
            move |uuid, mut l| {
                Box::pin({
                    let toggl_api_token = settings.toggl_api_token.clone();
                    let spreadsheet_id = settings.google_sheets_spreadsheet_id.clone();
                    let google_authenticator = google_authenticator.clone();
                    async move {
                        info!("Starting scheduled sync...");
                        if let Err(e) =
                            run_sync_task(&toggl_api_token, &spreadsheet_id, google_authenticator)
                                .await
                        {
                            error!("Error during scheduled sync: {}", e);
                        }
                        info!("Scheduled sync finished.");

                        // Query the next execution time for this job
                        let next_tick = l.next_tick_for_job(uuid).await;
                        if let Ok(Some(ts)) = next_tick {
                            info!("Next time for job is {:?}", ts);
                        } else {
                            info!("Could not get next tick for job");
                        }
                    }
                })
            },
        )?)
        .await?;

    sched.start().await?;

    info!("Scheduler started.");

    // Keep the main thread alive
    loop {
        tokio::time::sleep(StdDuration::from_secs(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn test_serial_number_conversion() {
        let to_utc = |y, m, d, h, min, s| {
            NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(h, min, s)
                .unwrap()
                .and_utc()
        };

        // 1899-12-30 is 0.0
        assert!(
            (to_serial_number(to_utc(1899, 12, 30, 0, 0, 0)) - 0.0).abs() < 1e-6,
            "1899-12-30 should be 0.0"
        );

        // 1900-01-01 is 2.0
        assert!(
            (to_serial_number(to_utc(1900, 1, 1, 0, 0, 0)) - 2.0).abs() < 1e-6,
            "1900-01-01 should be 2.0"
        );

        // 1900-01-01 12:00:00 is 2.5
        assert!(
            (to_serial_number(to_utc(1900, 1, 1, 12, 0, 0)) - 2.5).abs() < 1e-6,
            "1900-01-01 noon should be 2.5"
        );

        // Unix Epoch: 1970-01-01 is 25569.0
        assert!(
            (to_serial_number(to_utc(1970, 1, 1, 0, 0, 0)) - 25569.0).abs() < 1e-6,
            "1970-01-01 should be 25569.0"
        );
    }
    #[test]
    fn test_extract_time_from_row() {
        // 1. Serial Number (f64)
        let row_serial = vec![serde_json::json!(25569.0)]; // 1970-01-01
        assert!((extract_time_from_row(&row_serial) - 25569.0).abs() < 1e-6);

        // 2. RFC3339 String
        let row_rfc3339 = vec![serde_json::json!("1970-01-01T00:00:00+00:00")];
        assert!((extract_time_from_row(&row_rfc3339) - 25569.0).abs() < 1e-6);

        // 3. "YYYY-MM-DD HH:MM:SS UTC" String
        let row_utc_suffix = vec![serde_json::json!("1970-01-01 00:00:00 UTC")];
        assert!((extract_time_from_row(&row_utc_suffix) - 25569.0).abs() < 1e-6);

        // 4. "YYYY-MM-DD HH:MM:SS" String
        let row_no_suffix = vec![serde_json::json!("1970-01-01 00:00:00")];
        assert!((extract_time_from_row(&row_no_suffix) - 25569.0).abs() < 1e-6);

        // 5. Invalid String
        let row_invalid = vec![serde_json::json!("invalid-date")];
        assert_eq!(extract_time_from_row(&row_invalid), 0.0);

        // 6. Empty Row
        let row_empty: Vec<serde_json::Value> = vec![];
        assert_eq!(extract_time_from_row(&row_empty), 0.0);

        // 7. Non-date first column
        let row_other = vec![serde_json::json!(true)];
        assert_eq!(extract_time_from_row(&row_other), 0.0);
    }
}
