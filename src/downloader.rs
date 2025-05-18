use crate::Streamer;
use crossbeam::channel::{Receiver, Sender};
use jiff::{Timestamp, tz::TimeZone};
use std::fmt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub struct HeartbeatError {
    details: String,
}

impl HeartbeatError {
    pub fn new() -> Self {
        Self {
            details: "No Heartbeat Received!".to_string(),
        }
    }
}

impl std::error::Error for HeartbeatError {}

impl fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

#[derive(Debug)]
pub struct DownloaderError {
    details: String,
}

impl DownloaderError {
    pub fn new(msg: &str) -> Self {
        Self {
            details: msg.to_string(),
        }
    }
}

impl std::error::Error for DownloaderError {}

impl fmt::Display for DownloaderError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

pub enum StreamerAction {
    Heartbeat,
    Start(String),
    Stop,
    StopAll,
}

pub struct StreamerUpdate {
    pub streamer: Streamer,
    pub action: StreamerAction,
}

pub struct DownloaderProc {
    m3u8_url: String,
    name: String,
    download_file: PathBuf,
    handle: Option<Child>,
}

impl DownloaderProc {
    fn new(url: &str, name: &str) -> DownloaderProc {
        let out_path = String::from("/tmp");
        let filename = format!(
            "{}/{}",
            out_path,
            Timestamp::now()
                .to_zoned(TimeZone::system())
                .strftime("%H_%M_%S-%m_%d_%Y")
        );
        DownloaderProc {
            m3u8_url: String::from(url),
            name: String::from(name),
            download_file: filename.into(),
            handle: None,
        }
    }

    fn get_proc_status(&mut self) -> Option<ExitStatus> {
        if let Some(proc) = self.handle.as_mut() {
            return proc.try_wait().unwrap();
        } else {
            return None;
        }
    }

    fn start_downloading(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let proc = Command::new("ffmpeg")
            .arg("-i")
            .arg(&self.m3u8_url)
            .arg("-c")
            .arg("copy")
            .arg(self.download_file.as_path())
            .spawn()?;
        self.handle = Some(proc);
        Ok(())
    }

    fn stop_downloading(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(proc) = self.handle.as_mut() {
            match proc.kill() {
                Ok(()) => {
                    return Ok(());
                }
                Err(e) => {
                    println!("COULDN'T STOP PROCESS");
                    return Err(e.into());
                }
            }
        } else {
            return Err(Box::new(DownloaderError::new(
                "No Active Process for Downloading User",
            )));
        }
    }
}

pub fn download_manager(
    rx_channel: Arc<Receiver<StreamerUpdate>>,
    tx_channel: Arc<Sender<StreamerUpdate>>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Downloader Thread");
    let mut isrunning = true;
    // Create hashmap here
    let mut downloaders: Vec<DownloaderProc> = Vec::new();
    tx_channel.send(StreamerUpdate {
        streamer: Streamer::default(),
        action: StreamerAction::Heartbeat,
    })?;
    while isrunning {
        // Check Status of all currently managed processes
        // Check for any messages
        let mut to_remove = Vec::new();
        for (idx, downloader) in downloaders.iter_mut().enumerate() {
            if downloader.get_proc_status().is_some() {
                to_remove.push(idx);
            }
        }

        for idx in to_remove {
            downloaders.remove(idx);
        }

        for msg in rx_channel.try_iter() {
            println!("Handling Message for {}.", msg.streamer.profile_name);
            match msg.action {
                StreamerAction::Start(m3u8) => {
                    let mut new_downloader = DownloaderProc::new(&m3u8, &msg.streamer.profile_name);
                    new_downloader.start_downloading()?;
                    downloaders.push(new_downloader);
                }
                StreamerAction::Stop => {
                    // find the downloader in the list, stop it, and remove it
                    downloaders
                        .iter_mut()
                        .find(|dl| dl.name == msg.streamer.profile_name)
                        .unwrap()
                        .stop_downloading()?;
                }
                StreamerAction::StopAll => {
                    isrunning = false;
                    downloaders
                        .iter_mut()
                        .for_each(|dl| dl.stop_downloading().unwrap());
                    downloaders.clear();
                    // Break the for loop
                    break;
                }
                _ => {
                    println!("ERROR! Heartbeat received erroneously");
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}
