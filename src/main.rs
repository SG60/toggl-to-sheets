use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use dotenv::dotenv;
use google_sheets4::{api::ValueRange, hyper_rustls, hyper_util, yup_oauth2, Sheets};
use serde::Deserialize;
use std::env;
use std::error::Error;

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
    project_name: Option<serde_json::Value>,
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
            // include meta entity data in the response (e.g. project_name etc.)
            ("meta", "true".to_string()),
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

// Helper to convert DateTime<Utc> to Google Sheets serial number (days since Dec 30, 1899)
fn to_serial_number(t: DateTime<Utc>) -> f64 {
    let epoch = NaiveDateTime::parse_from_str("1899-12-30 00:00:00", "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_utc();
    let duration = t.signed_duration_since(epoch);
    // Days + fraction of day
    duration.num_milliseconds() as f64 / 86_400_000.0
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

    // Check if the sheet exists
    let spreadsheet = hub.spreadsheets().get(spreadsheet_id).doit().await?.1;
    let sheet_exists = spreadsheet
        .sheets
        .as_ref()
        .map(|sheets| {
            sheets.iter().any(|s| {
                s.properties
                    .as_ref()
                    .map_or(false, |p| p.title.as_deref() == Some(GOOGLE_SHEET_NAME))
            })
        })
        .unwrap_or(false);

    if !sheet_exists {
        println!("Sheet '{}' not found. Creating it...", GOOGLE_SHEET_NAME);
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
        println!("Sheet '{}' created.", GOOGLE_SHEET_NAME);
    }

    // 2. Read existing data
    // We use UNFORMATTED_VALUE to get numbers for dates if they are already in serial format
    let range = format!("'{}'!A:F", GOOGLE_SHEET_NAME);
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
            println!("Read {} existing rows from Sheet.", rows.len());
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

    println!("Kept {} rows older than {}.", kept_rows.len(), cutoff_date);

    // 3. Convert new Toggl entries to Rows
    let mut new_rows: Vec<Vec<serde_json::Value>> = Vec::new();
    for entry in new_entries {
        let duration_mins = entry.duration as f64 / 60.0;

        let stop_val = match entry.stop {
            Some(t) => serde_json::json!(to_serial_number(t)),
            None => serde_json::json!("Running"),
        };

        let tags_str = entry.tags.unwrap_or_default().join(", ");

        // Use Serial Number for start time
        let row = vec![
            serde_json::json!(to_serial_number(entry.start)),
            serde_json::json!(entry.description.unwrap_or("".into())),
            serde_json::json!(duration_mins),
            serde_json::json!(entry.project_id.map_or("".into(), |id| id.to_string())),
            serde_json::json!(entry.project_name.unwrap_or("".into())),
            serde_json::json!(tags_str),
            stop_val,
        ];
        new_rows.push(row);
    }

    println!("Adding {} new entries.", new_rows.len());

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
            &format!("'{}'!A:F", GOOGLE_SHEET_NAME),
        )
        .doit()
        .await?;

    // 6. Write All Data
    if !all_rows.is_empty() {
        let req = ValueRange {
            values: Some(all_rows),
            ..Default::default()
        };

        hub.spreadsheets()
            .values_update(req, spreadsheet_id, &format!("'{}'!A1", GOOGLE_SHEET_NAME))
            .value_input_option("USER_ENTERED") // Important for Sheets to recognize numbers as potential dates
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
