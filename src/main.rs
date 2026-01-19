use anyhow::Result;
use chrono::{DateTime, Utc};
use dotenvy::dotenv;
use log::{error, info, warn};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use simplelog::*;
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// --- CONFIGURATION STRUCTS ---
#[derive(Deserialize)]
struct Album {
    id: String,
    #[serde(rename = "albumName")]
    album_name: String,
}

#[derive(Deserialize)]
struct AssetResponse {
    id: String,
}

const HISTORY_FILE: &str = "immich_upload_history.json";
const LOG_FILE: &str = "immich_backup.log";
const DEVICE_ID: &str = "rust-uploader-v1";

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Setup Logging (Console + File)
    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            LevelFilter::Info,
            Config::default(),
            File::create(LOG_FILE).expect("Failed to create log file"),
        ),
    ])?;

    // 2. Load .env
    dotenv().ok();
    let folder = env::var("SCREENSHOTS_PATH").expect("SCREENSHOTS_PATH not set");
    let api_key = env::var("IMMICH_API_KEY").expect("IMMICH_API_KEY not set");
    let local_url = env::var("IMMICH_LOCAL_URL").unwrap_or_default();
    let ext_url = env::var("IMMICH_EXTERNAL_URL").unwrap_or_default();
    let album_name = env::var("IMMICH_ALBUM_NAME").expect("IMMICH_ALBUM_NAME not set");

    let client = Client::builder().timeout(Duration::from_secs(60)).build()?;

    // 3. Network Detection
    let base_url = match get_active_url(&client, &local_url, &ext_url).await {
        Some(url) => url,
        None => {
            error!("Could not connect to any Immich instance.");
            return Ok(());
        }
    };

    // 4. Find Album ID
    info!("Looking for album: '{}'...", album_name);
    let album_id = match get_album_id(&client, &base_url, &api_key, &album_name).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            error!("Album '{}' not found on server!", album_name);
            return Ok(());
        }
        Err(e) => {
            error!("Error fetching albums: {:?}", e);
            return Ok(());
        }
    };

    // 5. Load History
    let mut history = load_history()?;
    let path = Path::new(&folder);
    if !path.exists() {
        error!("Screenshots folder not found: {}", folder);
        return Ok(());
    }

    // 6. Process Files
    let mut entries: Vec<PathBuf> = fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            if let Some(ext) = p.extension() {
                let s = ext.to_string_lossy().to_lowercase();
                matches!(s.as_str(), "png" | "jpg" | "jpeg" | "webp")
            } else {
                false
            }
        })
        .collect();

    // Sort by modification time (Oldest first)
    entries.sort_by_key(|p| p.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH));

    let mut count = 0;
    for file_path in entries {
        let filename = file_path.file_name().unwrap().to_string_lossy().to_string();

        if history.contains(&filename) {
            continue;
        }

        info!("Uploading: {}...", filename);
        match upload_asset(&client, &file_path, &base_url, &api_key).await {
            Ok(Some(asset_id)) => {
                if asset_id != "DUPLICATE_UNKNOWN_ID" {
                    if let Err(e) = add_to_album(&client, &base_url, &api_key, &album_id, &asset_id).await {
                        error!("Failed to link to album: {:?}", e);
                    }
                }
                history.insert(filename.clone());
                save_history(&history)?;
                count += 1;
            }
            Ok(None) => { /* Failed, do nothing (will retry next time) */ }
            Err(e) => error!("Upload error for {}: {:?}", filename, e),
        }
    }

    if count > 0 {
        info!("Done! Processed {} images.", count);
    } else {
        info!("No new screenshots found.");
    }

    Ok(())
}

// --- HELPER FUNCTIONS ---

async fn get_active_url(client: &Client, local: &str, external: &str) -> Option<String> {
    if !local.is_empty() {
        info!("Checking connection to: {}...", local);
        // Updated API endpoint
        if client.get(format!("{}/api/server/ping", local)).timeout(Duration::from_secs(2)).send().await.is_ok() {
            info!("Local network detected.");
            return Some(local.to_string());
        }
    }
    
    if !external.is_empty() {
        info!("Switching to External URL.");
        return Some(external.to_string());
    }
    None
}

async fn get_album_id(client: &Client, base_url: &str, key: &str, name: &str) -> Result<Option<String>> {
    let url = format!("{}/api/albums", base_url);
    let resp = client.get(&url).header("x-api-key", key).send().await?;
    resp.error_for_status_ref()?;
    
    let albums: Vec<Album> = resp.json().await?;
    for album in albums {
        if album.album_name == name {
            return Ok(Some(album.id));
        }
    }
    Ok(None)
}

async fn add_to_album(client: &Client, base_url: &str, key: &str, album_id: &str, asset_id: &str) -> Result<()> {
    let url = format!("{}/api/albums/{}/assets", base_url, album_id);
    let body = serde_json::json!({ "ids": [asset_id] });
    
    client.put(&url)
        .header("x-api-key", key)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
        
    info!("   -- Added to album");
    Ok(())
}

async fn upload_asset(client: &Client, path: &Path, base_url: &str, key: &str) -> Result<Option<String>> {
    let filename = path.file_name().unwrap().to_string_lossy();
    let metadata = fs::metadata(path)?;
    let size = metadata.len();
    
    // Create timestamps in strict ISO format for Immich
    let created: DateTime<Utc> = metadata.created().unwrap_or(SystemTime::now()).into();
    let modified: DateTime<Utc> = metadata.modified().unwrap_or(SystemTime::now()).into();
    
    let device_asset_id = format!("{}-{}-{}", filename, size, modified.timestamp());

    // Prepare multipart form
    let file_bytes = tokio::fs::read(path).await?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    
    let part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(filename.to_string())
        .mime_str(mime.as_ref())?;

    let form = reqwest::multipart::Form::new()
        .part("assetData", part)
        .text("deviceAssetId", device_asset_id)
        .text("deviceId", DEVICE_ID)
        .text("fileCreatedAt", created.to_rfc3339())
        .text("fileModifiedAt", modified.to_rfc3339())
        .text("isFavorite", "false");

    let resp = client.post(format!("{}/api/assets", base_url))
        .header("x-api-key", key)
        .multipart(form)
        .send()
        .await?;

    let status = resp.status();

    if status == StatusCode::CREATED {
        let json: AssetResponse = resp.json().await?;
        Ok(Some(json.id))
    } else if status == StatusCode::OK {
        warn!("File exists (Deduplicated): {}", filename);
        let json: AssetResponse = resp.json().await?;
        Ok(Some(json.id))
    } else if status == StatusCode::CONFLICT {
        warn!("Duplicate rejected: {}", filename);
        // Try to parse ID from error body if possible, otherwise return generic flag
        match resp.json::<AssetResponse>().await {
            Ok(json) => Ok(Some(json.id)),
            Err(_) => Ok(Some("DUPLICATE_UNKNOWN_ID".to_string()))
        }
    } else {
        let error_text = resp.text().await?;
        error!("Upload failed for {}: Status {} - {}", filename, status, error_text);
        Ok(None)
    }
}

fn load_history() -> Result<HashSet<String>> {
    if Path::new(HISTORY_FILE).exists() {
        let file = File::open(HISTORY_FILE)?;
        let history: Vec<String> = serde_json::from_reader(file).unwrap_or_default();
        return Ok(history.into_iter().collect());
    }
    Ok(HashSet::new())
}

fn save_history(history: &HashSet<String>) -> Result<()> {
    let file = File::create(HISTORY_FILE)?;
    let list: Vec<&String> = history.iter().collect();
    serde_json::to_writer_pretty(file, &list)?;
    Ok(())
}