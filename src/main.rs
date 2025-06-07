use crossbeam::channel::{Receiver, Sender, unbounded};
use env_logger::Builder;
use log::{debug, error, info, trace};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::thread;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::sleep;

pub mod downloader;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum StreamerState {
    #[serde(rename = "added")]
    Added,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "downloading")]
    Downloading,
    #[serde(rename = "stopped")]
    Stopped,
    #[serde(rename = "error")]
    Error(u32),
}

enum OnlineState {
    Online(String),
    Offline,
}

#[derive(Deserialize, Serialize)]
pub struct Streamer {
    profile_url: String,
    profile_name: String,
    profile_status: StreamerState,
    download_size_mb: u32,
}

impl Default for Streamer {
    fn default() -> Self {
        Streamer {
            profile_url: "N/A".to_string(),
            profile_name: "N/A".to_string(),
            profile_status: StreamerState::Stopped,
            download_size_mb: 0,
        }
    }
}

async fn obtain_user_status(
    streamer: &Streamer,
    client: &Client,
    room_regex: &Regex,
) -> Result<OnlineState, Box<dyn std::error::Error>> {
    // TODO: This can't be unwrapped as there can be any number of errors with it
    let streamer_resp = client
        .get(streamer.profile_url.clone())
        .send()
        .await
        .unwrap();

    let client_html = streamer_resp.text().await?;

    let dossier_json_str: &str = room_regex
        .captures(&client_html)
        .and_then(|captures| captures.get(1))
        .expect("No Valid Regex Found")
        .as_str();
    let json_obj: Value =
        serde_json::from_str(&dossier_json_str).expect("Invalid JSON Object Received");

    let dossier_obj: Value =
        serde_json::from_str(&json_obj.as_str().unwrap()).expect("Invalid Dossier JSON");
    let mut m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
    m3u8_link.retain(|c| c != '"');

    if m3u8_link.len() > 2 && m3u8_link.ends_with("m3u8") {
        Ok(OnlineState::Online(m3u8_link))
    } else {
        Ok(OnlineState::Offline)
    }
}

// Main status loop
// 1. Query for any users manually stopped and kill their downloads
// 2. Query all waiting users, check if any of them came online
// 3. If any of the waiting users are online start a downloader thread and update their state in the DB
async fn status_loop(
    rx_channel: Arc<Receiver<downloader::StreamerUpdate>>,
    tx_channel: Arc<Sender<downloader::StreamerUpdate>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rx = Arc::clone(&rx_channel);
    info!("waiting for heartbeat from downloader thread");
    let hb_task = tokio::task::spawn_blocking(move || {
        let hb = rx.recv().unwrap();
        let mut hb_received: bool = false;
        match hb.action {
            downloader::StreamerAction::Heartbeat => {
                trace!("Heartbeat Received! Starting");
                hb_received = true;
            }
            _ => {
                error!("No heartbeat received!");
            }
        }
        return hb_received;
    });
    if !hb_task.await? {
        return Err(Box::new(downloader::HeartbeatError::new()));
    }
    info!("heartbeat received from downloader thread, starting main loop");

    let streamer_api_client = Client::new();
    let streamer_statuser_client = Client::new();
    let room_regex: Regex = Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#)?;
    loop {
        // TODO: Need to detect when a user has been manually stopped correctly, I think empty requests are failing
        debug!("get stopped users ...");
        let stopped_users_resp = streamer_api_client
            .get("http://127.0.0.1:8000/users/state/stopped")
            .send()
            .await
            .unwrap();
        // Handle this, don't just unwrap
        let stopped_users: Vec<Streamer> =
            stopped_users_resp.json::<Vec<Streamer>>().await.unwrap();
        for user in stopped_users {
            tx_channel.send(downloader::StreamerUpdate {
                streamer: user,
                action: downloader::StreamerAction::Stop,
            })?;
        }
        debug!("get waiting users ...");
        let waiting_users_resp = streamer_api_client
            .get("http://127.0.0.1:8000/users/state/waiting")
            .send()
            .await
            .unwrap();
        // Handle this, don't just unwrap
        let waiting_users: Vec<Streamer> =
            waiting_users_resp.json::<Vec<Streamer>>().await.unwrap();
        for user in waiting_users {
            debug!("statusing: {}", user.profile_name);
            // Handle This Error then handle whether or not they're online
            let state: OnlineState =
                obtain_user_status(&user, &streamer_statuser_client, &room_regex)
                    .await
                    .unwrap();
            match state {
                OnlineState::Online(m3u8) => {
                    debug!("user {} is online. link: {}", user.profile_name, m3u8);
                    let new_user_status = Streamer {
                        profile_name: user.profile_name.clone(),
                        profile_url: user.profile_url.clone(),
                        profile_status: StreamerState::Downloading,
                        download_size_mb: 0,
                    };
                    streamer_api_client
                        .post(format!("http://127.0.0.1:8000/users/{}", user.profile_name))
                        .json(&new_user_status)
                        .send()
                        .await
                        .unwrap();
                    debug!("Send Message to Downloader Thread");
                    tx_channel.send(downloader::StreamerUpdate {
                        streamer: new_user_status,
                        action: downloader::StreamerAction::Start(m3u8.clone()),
                    })?;
                }
                OnlineState::Offline => trace!("Offline"),
            }
            sleep(tokio::time::Duration::from_secs(5)).await;
        }
        sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Builder::new()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Stdout)
        .init();
    let (statuser_tx, downloader_rx) = unbounded::<downloader::StreamerUpdate>();
    let (downloader_tx, statuser_rx) = unbounded::<downloader::StreamerUpdate>();
    let dl_tx_ptr = Arc::new(downloader_tx);
    let dl_rx_ptr = Arc::new(downloader_rx);
    let downloader_thread_handle = thread::spawn(move || {
        let _res = downloader::download_manager(dl_rx_ptr, dl_tx_ptr);
    });
    let status_tx_ptr = Arc::new(statuser_tx);
    let status_rx_ptr = Arc::new(statuser_rx);
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        ret = status_loop(status_rx_ptr, status_tx_ptr.clone()) => {
            if let Err(e) = ret {
                error!("Status Loop Crashed! {}", e);
                return Err(e);
            }
        }
        _ = async {
            tokio::select! {
                _ = sigint.recv() => warn!("received SIGINT"),
                _ = sigterm.recv() => warn!("received SIGTERM"),
            }
        } => {
            info!("Entering Shutdown State");
            status_tx_ptr.send(downloader::StreamerUpdate{
                streamer: Streamer {
                        profile_url: "N/A".to_string(),
                        profile_name: "N/A".to_string(),
                        profile_status: StreamerState::Stopped,
                        download_size_mb: 0 },
                action: downloader::StreamerAction::StopAll
            })?;
        }
    }
    let _thread_res = downloader_thread_handle.join();
    Ok(())
}
#[cfg(test)]
#[test]
fn online_user_test() {
    use regex::Regex;
    use serde_json::Value;
    use std::fs;

    let online_user_html: String =
        fs::read_to_string("./html/online.html").expect("Could Not Read HTML File");

    let room_regex: Regex = Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#)
        .expect("Invalid Room Regex");

    let dossier_json_str: &str = room_regex
        .captures(&online_user_html)
        .and_then(|captures| captures.get(1))
        .expect("No Valid Regex Found")
        .as_str();
    let json_obj: Value =
        serde_json::from_str(&dossier_json_str).expect("Invalid JSON Object Received");

    let dossier_obj: Value =
        serde_json::from_str(&json_obj.as_str().unwrap()).expect("Invalid Dossier JSON");
    assert!(dossier_obj.get("hls_source").is_some());

    let mut m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
    // Strip the quotations off the string
    m3u8_link.retain(|c| c != '"');
    println!("{}", m3u8_link);

    assert!(!m3u8_link.is_empty());
    assert!(m3u8_link.ends_with(".m3u8"));
}

#[test]
fn offline_user_test() {
    use regex::Regex;
    use serde_json::Value;
    use std::fs;

    let online_user_html: String =
        fs::read_to_string("./html/offline.html").expect("Could Not Read HTML File");

    let room_regex: Regex = Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#)
        .expect("Invalid Room Regex");

    let dossier_json_str: &str = room_regex
        .captures(&online_user_html)
        .and_then(|captures| captures.get(1))
        .expect("No Valid Regex Found")
        .as_str();
    let json_obj: Value =
        serde_json::from_str(&dossier_json_str).expect("Invalid JSON Object Received");

    let dossier_obj: Value =
        serde_json::from_str(&json_obj.as_str().unwrap()).expect("Invalid Dossier JSON");
    assert!(dossier_obj.get("hls_source").is_some());

    let mut m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
    m3u8_link.retain(|c| c != '"');
    println!("{}", m3u8_link);

    assert!(m3u8_link.is_empty());
}
