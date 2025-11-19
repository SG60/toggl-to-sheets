use chrono::{DateTime, Utc};
use dotenv::dotenv;
use google_sheets4::{api::ValueRange, hyper_rustls, hyper_util, yup_oauth2, Sheets};
use serde::Deserialize;
use std::env;
use std::error::Error;

// --- Data Structures ---

// Represents a Time Entry from Toggl's API (v9)
#[derive(Debug, Deserialize)]
struct TogglTimeEntry {
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

async fn append_to_sheet(
    spreadsheet_id: &str,
    entries: Vec<TogglTimeEntry>,
) -> Result<(), Box<dyn Error>> {
    if entries.is_empty() {
        println!("No entries to upload.");
        return Ok(());
    }

    // Get an ApplicationSecret instance by some means. It contains the `client_id` and
    // `client_secret`, among other things.
    let secret: yup_oauth2::ApplicationSecret = Default::default();
    // Instantiate the authenticator. It will choose a suitable authentication flow for you,
    // unless you replace  `None` with the desired Flow.
    // Provide your own `AuthenticatorDelegate` to adjust the way it operates and get feedback about
    // what's going on. You probably want to bring in your own `TokenStorage` to persist tokens and
    // retrieve them from storage.
    let auth = yup_oauth2::InstalledFlowAuthenticator::builder(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::HTTPRedirect,
    )
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

    // Transform Toggl entries into Rows (Vector of Strings)
    let mut values: Vec<Vec<serde_json::Value>> = Vec::new();

    for entry in entries {
        // Calculate duration in minutes (Toggl sends seconds)
        let duration_mins = entry.duration as f64 / 60.0;

        // Handle Optional Stop time
        let stop_time = match entry.stop {
            Some(t) => t.to_rfc3339(),
            None => "Running".to_string(),
        };

        // Flatten tags
        let tags_str = entry.tags.unwrap_or_default().join(", ");

        let row = vec![
            serde_json::json!(entry.start.to_string()), // Column A: Date/Time
            serde_json::json!(entry.description.unwrap_or_default()), // Column B: Description
            serde_json::json!(duration_mins),           // Column C: Duration (mins)
            serde_json::json!(entry.project_id.unwrap_or_default().to_string()), // Column D: Project ID
            serde_json::json!(tags_str),                                         // Column E: Tags
            serde_json::json!(stop_time), // Column F: Stop Time
        ];
        values.push(row);
    }

    let req = ValueRange {
        values: Some(values),
        ..Default::default()
    };

    // "Sheet1!A1" tells Google to look at Sheet1 and append after the last data found
    let range = "Sheet1!A1";

    let result = hub
        .spreadsheets()
        .values_append(req, spreadsheet_id, range)
        .value_input_option("USER_ENTERED") // Parses numbers and dates automatically
        .doit()
        .await;

    match result {
        Ok(_) => println!("Successfully appended data to Google Sheet."),
        Err(e) => eprintln!("Error writing to Google Sheets: {}", e),
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

    // 2. Define Time Range (e.g., Last 24 hours)
    // You can adjust this logic to fetch "Yesterday", "Last Week", etc.
    let end_date = Utc::now();
    let start_date = end_date - chrono::Duration::hours(24);

    println!("Time Range: {} to {}", start_date, end_date);

    // 3. Fetch from Toggl
    let entries = fetch_toggl_entries(&toggl_token, start_date, end_date).await?;

    // 4. Push to Google Sheets
    append_to_sheet(&spreadsheet_id, entries).await?;

    Ok(())
}
