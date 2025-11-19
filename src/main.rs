use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use dotenv::dotenv;
use google_sheets4::{api::ValueRange, hyper_rustls, hyper_util, yup_oauth2, Sheets};
use serde::Deserialize;
use std::env;
use std::error::Error;

// --- Data Structures ---

// Represents a Time Entry from Toggl's API (v9)
#[derive(Debug, Deserialize)]
struct TogglTimeEntry {
    #[allow(dead_code)]
    id: i64,
    description: Option<String>,
    start: DateTime<Utc>,
    stop: Option<DateTime<Utc>>,
    duration: i64,
    project_id: Option<i64>,
    tags: Option<Vec<String>>,
}

// --- Toggl Logic ---

async fn fetch_toggl_entries(
    api_token: &str,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
) -> Result<Vec<TogglTimeEntry>, Box<dyn Error>> {
    let client = reqwest::Client::new();

    // Toggl v9 API Endpoint for "me/time_entries"
    let url = "https://api.track.toggl.com/api/v9/me/time_entries";

    println!("Fetching Toggl entries...");

    let response = client
        .get(url)
        .basic_auth(api_token, Some("api_token"))
        .query(&[
            ("start_date", start_date.to_rfc3339()),
            ("end_date", end_date.to_rfc3339()),
        ])
        .send()
        .await?;

    if !response.status().is_success() {
        let error_text = response.text().await?;
        return Err(format!("Toggl API Error: {}", error_text).into());
    }

    let entries: Vec<TogglTimeEntry> = response.json().await?;
    println!("Found {} entries.", entries.len());

    Ok(entries)
}

// --- Google Sheets Logic ---

async fn sync_sheet(
    spreadsheet_id: &str,
    new_entries: Vec<TogglTimeEntry>,
    cutoff_date: DateTime<Utc>,
) -> Result<(), Box<dyn Error>> {
    // 1. Authenticate
    let secret: yup_oauth2::ApplicationSecret =
        yup_oauth2::read_application_secret("google_clientsecret.json")
            .await
            .expect("google_clientsecret.json");

    let auth = yup_oauth2::InstalledFlowAuthenticator::builder(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::HTTPRedirect,
    )
    .persist_tokens_to_disk("google_tokencache.json")
    .build()
    .await
    .unwrap();

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .build(),
        );
    let hub = Sheets::new(client, auth);

    // 2. Read existing data
    let range = "Sheet1!A:F";
    let result = hub
        .spreadsheets()
        .values_get(spreadsheet_id, range)
        .doit()
        .await;

    let mut kept_rows: Vec<Vec<serde_json::Value>> = Vec::new();

    if let Ok((_, value_range)) = result {
        if let Some(rows) = value_range.values {
            println!("Read {} existing rows from Sheet.", rows.len());
            for row in rows {
                // Try to parse the date from the first column (index 0)
                let should_keep = if let Some(date_val) = row.get(0) {
                    if let Some(date_str) = date_val.as_str() {
                        // Try parsing with default to_string format first, then RFC3339
                        // The previous code used entry.start.to_string() which is usually "%Y-%m-%d %H:%M:%S %Z"
                        // But chrono's default Display might vary.
                        // Let's try a few common formats.

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
                            // Could not parse date, keep the row (safe default, e.g. header)
                            true
                        }
                    } else {
                        true // Not a string, keep it
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

    println!("Kept {} rows older than {}.", kept_rows.len(), cutoff_date);

    // 3. Convert new Toggl entries to Rows
    let mut new_rows: Vec<Vec<serde_json::Value>> = Vec::new();
    for entry in new_entries {
        let duration_mins = entry.duration as f64 / 60.0;
        let stop_time = match entry.stop {
            Some(t) => t.to_rfc3339(),
            None => "Running".to_string(),
        };
        let tags_str = entry.tags.unwrap_or_default().join(", ");

        // Use RFC3339 for new entries for better machine readability
        let row = vec![
            serde_json::json!(entry.start.to_rfc3339()),
            serde_json::json!(entry.description.unwrap_or_default()),
            serde_json::json!(duration_mins),
            serde_json::json!(entry.project_id.unwrap_or_default().to_string()),
            serde_json::json!(tags_str),
            serde_json::json!(stop_time),
        ];
        new_rows.push(row);
    }

    println!("Adding {} new entries.", new_rows.len());

    // 4. Combine Rows
    kept_rows.append(&mut new_rows);

    // 5. Clear Sheet
    // We clear everything to ensure no stale data remains if the total row count decreases
    let clear_req = google_sheets4::api::ClearValuesRequest::default();
    hub.spreadsheets()
        .values_clear(clear_req, spreadsheet_id, "Sheet1!A:F")
        .doit()
        .await?;

    // 6. Write All Data
    if !kept_rows.is_empty() {
        let req = ValueRange {
            values: Some(kept_rows),
            ..Default::default()
        };

        hub.spreadsheets()
            .values_update(req, spreadsheet_id, "Sheet1!A1")
            .value_input_option("USER_ENTERED")
            .doit()
            .await?;
        println!("Successfully synced data to Google Sheet.");
    } else {
        println!("No data to write.");
    }

    Ok(())
}

// --- Main Execution ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenv().ok();

    // 1. Load Config
    let toggl_token = env::var("TOGGL_API_TOKEN").expect("TOGGL_API_TOKEN must be set");
    let spreadsheet_id =
        env::var("GOOGLE_SHEETS_SPREADSHEET_ID").expect("GOOGLE_SHEETS_SPREADSHEET_ID must be set");

    // 2. Define Time Range (Last 7 days)
    let end_date = Utc::now();
    let start_date = end_date - Duration::weeks(1);

    println!("Time Range: {} to {}", start_date, end_date);

    // 3. Fetch from Toggl
    let entries = fetch_toggl_entries(&toggl_token, start_date, end_date).await?;

    // 4. Sync to Google Sheets
    // We use start_date as the cutoff: anything in the sheet >= start_date is replaced.
    sync_sheet(&spreadsheet_id, entries, start_date).await?;

    Ok(())
}
