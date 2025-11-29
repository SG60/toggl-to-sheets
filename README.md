# Toggl Track to Google Sheets sychronisation

This is a Rust program that synchronises Toggl Track data to Google Sheets.

Some references:
- [Toggl Track API](https://engineering.toggl.com/docs/api/time_entries)
- [Google Sheets API](https://developers.google.com/workspace/sheets/api/guides/concepts)

## Sharing the Google Sheet with the Service Account

To share the Google Sheet with the Service Account, follow these steps:

1. Open the Google Sheet you want to share.
2. Click on the "Share" button in the top right corner.
3. Enter the email address of the Service Account that was created in the GCP Console (e.g., `toggl-to-sheets@toggl-to-sheets-12345.iam.gserviceaccount.com`) in the "Enter people or groups" field.
4. Select the appropriate permissions (e.g., "Editor") for the Service Account.
5. Click "Send" to share the sheet with the Service Account.

The GCP project containing the service account must have the Google Sheets API enabled.
