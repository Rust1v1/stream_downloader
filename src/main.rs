use crossbeam::channel::{Receiver, Sender, unbounded};
use env_logger::Builder;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use log::{debug, error, info, trace, warn};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::thread;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::sleep;

pub mod downloader;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DownloaderConfig {
    backend_url: String,
    download_dir: String,
    log_dir: String,
    enable_downloader: bool,
    max_parallel_downloads: usize,
    main_loop_sleep_time: u64,
    sleep_time_between_statusing: u64,
}

impl Default for DownloaderConfig {
    fn default() -> DownloaderConfig {
        DownloaderConfig {
            backend_url: String::from("http://127.0.0.1:8000"),
            download_dir: String::from("/tmp"),
            log_dir: String::from("/tmp"),
            enable_downloader: true,
            max_parallel_downloads: 2,
            main_loop_sleep_time: 5,
            sleep_time_between_statusing: 5,
        }
    }
}

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
    #[serde(rename = "stopping")]
    Stopping,
    #[serde(rename = "error")]
    Error(u32),
}

enum OnlineState {
    Online(String),
    Offline,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Streamer {
    profile_url: String,
    profile_name: String,
    profile_status: StreamerState,
    download_size_mb: u64,
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
        serde_json::from_str(dossier_json_str).expect("Invalid JSON Object Received");

    let dossier_obj: Value =
        serde_json::from_str(json_obj.as_str().unwrap()).expect("Invalid Dossier JSON");
    let mut m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
    m3u8_link.retain(|c| c != '"');

    if m3u8_link.len() > 2 && m3u8_link.ends_with("m3u8") {
        Ok(OnlineState::Online(m3u8_link))
    } else {
        Ok(OnlineState::Offline)
    }
}

async fn obtain_user_status_retry(
    streamer: &Streamer,
    client: &Client,
    room_regex: &Regex,
    retry_count: u32,
) -> Result<OnlineState, reqwest::Error> {
    let mut retries: u32 = 0;
    let base_delay_secs: u32 = 1;

    loop {
        match client.get(streamer.profile_url.clone()).send().await {
            Ok(streamer_resp) => {
                if streamer_resp.status().is_success() {
                    let client_html = streamer_resp.text().await?;

                    let dossier_json_str: &str = room_regex
                        .captures(&client_html)
                        .and_then(|captures| captures.get(1))
                        .expect("No Valid Regex Found")
                        .as_str();
                    let json_obj: Value = serde_json::from_str(dossier_json_str)
                        .expect("Invalid JSON Object Received");

                    let dossier_obj: Value = serde_json::from_str(json_obj.as_str().unwrap())
                        .expect("Invalid Dossier JSON");
                    let mut m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
                    m3u8_link.retain(|c| c != '"');

                    if m3u8_link.len() > 2 && m3u8_link.ends_with("m3u8") {
                        return Ok(OnlineState::Online(m3u8_link));
                    } else {
                        return Ok(OnlineState::Offline);
                    }
                } else {
                    warn!(
                        "Error Obtaining Status for User {}: {}",
                        streamer.profile_name,
                        streamer_resp.status()
                    );
                    if retries < retry_count {
                        retries += 1;
                        debug!("Trying to status that user again");
                        let delay = base_delay_secs * 2u32.pow(retries - 1);
                        tokio::time::sleep(tokio::time::Duration::from_secs(delay as u64)).await;
                    } else {
                        return Err(streamer_resp.error_for_status().unwrap_err());
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Error Obtaining Status for User {}: {}",
                    streamer.profile_name, e
                );
                if retries < retry_count {
                    retries += 1;
                    debug!("Trying to status that user again");
                    let delay = base_delay_secs * 2u32.pow(retries - 1);
                    tokio::time::sleep(tokio::time::Duration::from_secs(delay as u64)).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
}

async fn return_users_to_waiting(url: &str) {
    info!("setting all user states to waiting");
    let streamer_api_client = Client::new();
    streamer_api_client
        .post(url.to_string())
        .json(&json!({
            "profile_status": "waiting",
            "download_size_mb": 0,
        }))
        .send()
        .await
        .unwrap();
    info!("done");
}

// Main status loop
// 1. Query for any users manually stopped and kill their downloads
// 2. Query all waiting users, check if any of them came online
// 3. If any of the waiting users are online start a downloader thread and update their state in the DB
async fn status_loop(
    configuration: DownloaderConfig,
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
        hb_received
    });
    if !hb_task.await? {
        return Err(Box::new(downloader::HeartbeatError::new()));
    }
    info!("heartbeat received from downloader thread, starting main loop");

    let streamer_api_client = Client::new();
    let streamer_statuser_client = Client::new();
    let room_regex: Regex = Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#)?;
    let dl_rx = Arc::clone(&rx_channel);
    'main: loop {
        // TODO: Don't just crash here? Maybe loop until connection is made?
        let active_statusing: bool = streamer_api_client
            .get(format!("http://{}/statusing", configuration.backend_url))
            .send()
            .await?
            .text()
            .await?
            .parse::<bool>()?;
        trace!("get currently downloading users");
        let active_users_resp = streamer_api_client
            .get(format!(
                "http://{}/users/state/downloading",
                configuration.backend_url
            ))
            .send()
            .await
            .unwrap();

        let mut active_users: Vec<Streamer> =
            if let Ok(users) = active_users_resp.json::<Vec<Streamer>>().await {
                users
            } else {
                debug!("no currently downloading users");
                vec![]
            };

        trace!("currently active users: {:#?}", active_users);

        debug!("get stream updates from the downloader");
        let mut updates: Vec<downloader::StreamerUpdate> = vec![];
        while let Ok(update) = dl_rx.try_recv() {
            updates.push(update);
        }
        updates.sort_by(|a, b| {
            b.streamer
                .download_size_mb
                .cmp(&a.streamer.download_size_mb)
        });
        for user in active_users.iter() {
            if let Some(update) = updates
                .iter()
                .find(|u| u.streamer.profile_name == user.profile_name)
            {
                debug!("handling update for {}", update.streamer.profile_name);
                trace!("action: {:?}", update.action);
                let mut updated_streamer_status = user.clone();
                match update.action {
                    downloader::StreamerAction::Stop => {
                        updated_streamer_status.profile_status = StreamerState::Waiting
                    }
                    downloader::StreamerAction::Update => {
                        updated_streamer_status.download_size_mb = update.streamer.download_size_mb
                    }
                    _ => unreachable!(),
                }
                streamer_api_client
                    .post(format!(
                        "http://{}/users/{}",
                        configuration.backend_url,
                        update.streamer.profile_name.clone()
                    ))
                    .json(&updated_streamer_status)
                    .send()
                    .await?;
            } else {
                debug!("no updates received for user {}", user.profile_name);
            }
        }

        debug!("get users that have been manually requested to stop");
        let stopping_users_resp = streamer_api_client
            .get(format!(
                "http://{}/users/state/stopping",
                configuration.backend_url
            ))
            .send()
            .await
            .unwrap();

        let stopping_users: Vec<Streamer> =
            if let Ok(users) = stopping_users_resp.json::<Vec<Streamer>>().await {
                users
            } else {
                debug!("no currently stopped users");
                vec![]
            };

        for user in stopping_users {
            if let Some(user_idx) = active_users
                .iter()
                .position(|s| s.profile_name == user.profile_name)
            {
                active_users.remove(user_idx);
            } else {
                debug!(
                    "user {} was manually requested to stop but user has already been removed from active users list",
                    user.profile_name
                );
            }
            info!("Stopping user: {}", user.profile_name);
            tx_channel.send(downloader::StreamerUpdate {
                streamer: user.clone(),
                action: downloader::StreamerAction::Stop,
            })?;
            streamer_api_client
                .post(format!(
                    "http://{}/users/{}",
                    configuration.backend_url, user.profile_name
                ))
                .json(&Streamer {
                    profile_name: user.profile_name.clone(),
                    profile_url: user.profile_url.clone(),
                    profile_status: StreamerState::Stopped,
                    download_size_mb: 0,
                })
                .send()
                .await
                .unwrap();
        }

        if !active_statusing || active_users.len() >= configuration.max_parallel_downloads {
            debug!(
                "either statusing is disabled (enabled: {active_statusing}) or no available threads for downloaders, max threads: {}, active threads: {}.",
                active_users.len(),
                configuration.max_parallel_downloads
            );
            sleep(tokio::time::Duration::from_secs(
                configuration.main_loop_sleep_time,
            ))
            .await;
            continue 'main;
        }
        debug!("get waiting users ...");
        let waiting_users_resp = streamer_api_client
            .get(format!(
                "http://{}/users/state/waiting",
                configuration.backend_url
            ))
            .send()
            .await
            .unwrap();
        let waiting_users: Vec<Streamer> =
            if let Ok(users) = waiting_users_resp.json::<Vec<Streamer>>().await {
                users
            } else {
                debug!("no currently waiting users");
                vec![]
            };
        for user in waiting_users {
            debug!("statusing: {}", user.profile_name);
            // TODO: Handle This Error then handle whether or not they're online
            let state: OnlineState =
                obtain_user_status_retry(&user, &streamer_statuser_client, &room_regex, 4)
                    .await
                    .unwrap();
            match state {
                OnlineState::Online(m3u8) => {
                    info!("user {} is online. link: {}", user.profile_name, m3u8);
                    let new_user_status = Streamer {
                        profile_name: user.profile_name.clone(),
                        profile_url: user.profile_url.clone(),
                        profile_status: StreamerState::Downloading,
                        download_size_mb: 0,
                    };
                    streamer_api_client
                        .post(format!(
                            "http://{}/users/{}",
                            configuration.backend_url, user.profile_name
                        ))
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
                OnlineState::Offline => debug!(
                    "user {} is offline, away, or in private.",
                    user.profile_name
                ),
            }
            sleep(tokio::time::Duration::from_secs(
                configuration.sleep_time_between_statusing,
            ))
            .await;
        }
        sleep(tokio::time::Duration::from_secs(
            configuration.main_loop_sleep_time,
        ))
        .await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg: DownloaderConfig = Figment::new()
        .merge(Serialized::defaults(DownloaderConfig::default()))
        .merge(Toml::file("config.toml"))
        .merge(Env::prefixed("DL_"))
        .extract()
        .unwrap();
    Builder::new()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Stdout)
        .parse_default_env()
        .init();
    let (statuser_tx, downloader_rx) = unbounded::<downloader::StreamerUpdate>();
    let (downloader_tx, statuser_rx) = unbounded::<downloader::StreamerUpdate>();
    let dl_tx_ptr = Arc::new(downloader_tx);
    let dl_rx_ptr = Arc::new(downloader_rx);
    let dl_dir = cfg.download_dir.clone();
    let log_dir = cfg.log_dir.clone();
    let downloader_thread_handle = thread::spawn(move || {
        let _res = downloader::download_manager(dl_rx_ptr, dl_tx_ptr, &dl_dir, &log_dir);
    });
    let status_tx_ptr = Arc::new(statuser_tx);
    let status_rx_ptr = Arc::new(statuser_rx);
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let cfg_clone = cfg.clone();
    tokio::select! {
        ret = status_loop(cfg_clone, status_rx_ptr, status_tx_ptr.clone()) => {
            if let Err(e) = ret {
                error!("Status Loop Crashed! {}", e);
                status_tx_ptr.send(downloader::StreamerUpdate{
                    streamer: Streamer {
                            profile_url: "N/A".to_string(),
                            profile_name: "N/A".to_string(),
                            profile_status: StreamerState::Stopped,
                            download_size_mb: 0 },
                    action: downloader::StreamerAction::StopAll
                })?;
                return_users_to_waiting(&format!("http://{}/users", cfg.backend_url)).await;
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
            return_users_to_waiting(&format!("http://{}/users", cfg.backend_url)).await;
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

#[test]
fn test_message_handling() {
    let active_users: Vec<Streamer> = vec![
        Streamer {
            profile_name: "user1".to_string(),
            profile_url: "https://stream.site/user1".to_string(),
            profile_status: StreamerState::Downloading,
            download_size_mb: 123,
        },
        Streamer {
            profile_name: "user2".to_string(),
            profile_url: "https://stream.site/user2".to_string(),
            profile_status: StreamerState::Downloading,
            download_size_mb: 456,
        },
        Streamer {
            profile_name: "user3".to_string(),
            profile_url: "https://stream.site/user3".to_string(),
            profile_status: StreamerState::Downloading,
            download_size_mb: 1024,
        },
    ];

    trace!("currently active users: {:#?}", active_users);

    debug!("get stream updates from the downloader");
    let mut updates: Vec<downloader::StreamerUpdate> = vec![
        downloader::StreamerUpdate {
            streamer: Streamer {
                profile_name: "user1".to_string(),
                profile_url: "https://stream.site/user1".to_string(),
                profile_status: StreamerState::Downloading,
                download_size_mb: 150,
            },
            action: downloader::StreamerAction::Update,
        },
        downloader::StreamerUpdate {
            streamer: Streamer {
                profile_name: "user2".to_string(),
                profile_url: "https://stream.site/user2".to_string(),
                profile_status: StreamerState::Downloading,
                download_size_mb: 480,
            },
            action: downloader::StreamerAction::Update,
        },
        downloader::StreamerUpdate {
            streamer: Streamer {
                profile_name: "user1".to_string(),
                profile_url: "https://stream.site/user1".to_string(),
                profile_status: StreamerState::Downloading,
                download_size_mb: 170,
            },
            action: downloader::StreamerAction::Update,
        },
        downloader::StreamerUpdate {
            streamer: Streamer {
                profile_name: "user2".to_string(),
                profile_url: "https://stream.site/user2".to_string(),
                profile_status: StreamerState::Stopped,
                download_size_mb: 512,
            },
            action: downloader::StreamerAction::Stop,
        },
        downloader::StreamerUpdate {
            streamer: Streamer {
                profile_name: "user1".to_string(),
                profile_url: "https://stream.site/user1".to_string(),
                profile_status: StreamerState::Downloading,
                download_size_mb: 190,
            },
            action: downloader::StreamerAction::Update,
        },
    ];
    updates.sort_by(|a, b| {
        b.streamer
            .download_size_mb
            .cmp(&a.streamer.download_size_mb)
    });
    println!("{:#?}", updates);
    for user in active_users.iter() {
        if let Some(update) = updates
            .iter()
            .find(|u| u.streamer.profile_name == user.profile_name)
        {
            let mut updated_streamer_status = user.clone();
            match update.action {
                downloader::StreamerAction::Stop => {
                    updated_streamer_status.profile_status = StreamerState::Waiting
                }
                downloader::StreamerAction::Update => {
                    updated_streamer_status.download_size_mb = update.streamer.download_size_mb
                }
                _ => unreachable!(),
            }
            println!("{:#?}", updated_streamer_status);
        } else {
            println!("no update received for user {}", user.profile_name);
        }
    }
}
