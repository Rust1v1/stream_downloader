use crossbeam::channel::{Receiver, Sender, unbounded};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::thread;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Deserialize, Serialize)]
struct Streamer {
    profile_url: String,
    profile_name: String,
    profile_status: StreamerState,
    download_size_mb: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum StreamerState {
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

enum StreamerAction {
    Start,
    Stop,
}

struct StreamerUpdate {
    pub streamer: Streamer,
    pub action: StreamerAction,
}

enum OnlineState {
    Online(String),
    Offline,
}

fn download_manager(rx_channel: Arc<Receiver<StreamerUpdate>>, tx_channel: Arc<Sender<StreamerUpdate>>) -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Downloader Thread");
    loop {
        // Check Status of all currently managed processes
        // Check for any messages
        for msg in rx_channel.try_iter() {
            println!("Handling Message ... {} is being started", msg.streamer.profile_name);
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(())
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
// 1. Query all waiting users
async fn status_loop() -> Result<(), Box<dyn std::error::Error>> {
    let streamer_api_client = Client::new();
    let streamer_statuser_client = Client::new();
    let room_regex: Regex = Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#)?;
    loop {
        let waiting_users_resp = streamer_api_client
            .get("http://127.0.0.1:8000/users/state/waiting")
            .send()
            .await
            .unwrap();
        // Handle this, don't just unwrap
        let waiting_users: Vec<Streamer> =
            waiting_users_resp.json::<Vec<Streamer>>().await.unwrap();
        for user in waiting_users {
            println!("{}", user.profile_name);
            // Handle This Error then handle whether or not they're online
            let state: OnlineState =
                obtain_user_status(&user, &streamer_statuser_client, &room_regex)
                    .await
                    .unwrap();
            match state {
                OnlineState::Online(m3u8) => {
                    println!("User {} is online. Link: {}", user.profile_name, m3u8);
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
                }
                OnlineState::Offline => println!("Offline"),
            }
            sleep(tokio::time::Duration::from_secs(10)).await;
        }
        sleep(tokio::time::Duration::from_secs(5)).await;
        break;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (statuser_tx, downloader_rx) = unbounded::<StreamerUpdate>();
    let (downloader_tx, statuser_rx) = unbounded::<StreamerUpdate>();
    let dl_tx_ptr = Arc::new(downloader_tx);
    let dl_rx_ptr = Arc::new(downloader_rx);
    let downloader_thread_handle = thread::spawn(move || {
        let res = download_manager(dl_rx_ptr, dl_tx_ptr);
    });
    return status_loop().await;
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
