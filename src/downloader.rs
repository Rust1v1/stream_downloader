use crate::Streamer;
use crossbeam::channel::{Receiver, Sender};
use jiff::{Timestamp, tz::TimeZone};
use log::{debug, error, info, warn};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::fmt;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub struct HeartbeatError {
    details: String,
}

impl Default for HeartbeatError {
    fn default() -> Self {
        Self::new()
    }
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
        // fixme: this needs to be configurable
        let out_path = String::from("/tmp");
        let filename = format!(
            "{}/{}-{}.mp4",
            out_path,
            name,
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
            proc.try_wait().unwrap()
        } else {
            None
        }
    }

    fn start_downloading(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let stdout_file = File::create(format!("/tmp/{}-stdout.log", self.name))?;
        let stderr_file = File::create(format!("/tmp/{}-stderr.log", self.name))?;
        let proc = Command::new("ffmpeg")
            .arg("-i")
            .arg(&self.m3u8_url)
            .arg("-c")
            .arg("copy")
            .arg(self.download_file.as_path())
            .stdout(stdout_file)
            .stderr(stderr_file)
            .spawn()?;
        self.handle = Some(proc);
        Ok(())
    }

    fn stop_downloading(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("stopping downloader for {}", self.name);
        if let Some(proc) = self.handle.as_mut() {
            let pid = Pid::from_raw(proc.id() as i32);
            kill(pid, Signal::SIGINT)?;
            // After killing, sleep for 250 ms, check on the status, if it's not stopped kill it
            thread::sleep(Duration::from_millis(250));
            if proc.try_wait().unwrap().is_none() {
                match proc.kill() {
                    Ok(()) => {
                        warn!(
                            "SIGTERM didn't stop process so used SIGKILL for: {}",
                            self.name
                        );
                        Ok(())
                    }
                    Err(e) => {
                        error!("couldn't stop process");
                        Err(e.into())
                    }
                }
            } else {
                debug!("stopped downloader for: {}", self.name);
                Ok(())
            }
        } else {
            Err(Box::new(DownloaderError::new(
                "No Active Process for Downloading User",
            )))
        }
    }
}

pub fn download_manager(
    rx_channel: Arc<Receiver<StreamerUpdate>>,
    tx_channel: Arc<Sender<StreamerUpdate>>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("starting downloader thread");
    let mut isrunning = true;
    // Create hashmap here
    let mut downloaders: Vec<DownloaderProc> = Vec::new();
    tx_channel.send(StreamerUpdate {
        streamer: Streamer::default(),
        action: StreamerAction::Heartbeat,
    })?;
    while isrunning {
        debug!("{} streams running", downloaders.len());
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
            debug!("handling Message for {}.", msg.streamer.profile_name);
            match msg.action {
                StreamerAction::Start(m3u8) => {
                    let mut new_downloader = DownloaderProc::new(&m3u8, &msg.streamer.profile_name);
                    debug!("starting download process for {}", new_downloader.name);
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
                    // When this happens it seems borderline random whether or not there's any "known about" running streams. I have no idea what's happening to the downloaders vector. But the processes in them are successfully exiting.
                    // Actually what's probably happening is the child process is somehow consuming the control-c, stopping, then getting automatically removed from the vector. Don't know how that's happening
                    warn!("stop {} stream(s) and exit.", downloaders.len());
                    isrunning = false;
                    downloaders
                        .iter_mut()
                        .for_each(|dl| dl.stop_downloading().unwrap());
                    downloaders.clear();
                    // Break the for loop
                    break;
                }
                _ => {
                    error!("heartbeat received erroneously");
                }
            }
        }
        if isrunning {
            thread::sleep(Duration::from_secs(1));
        }
    }
    Ok(())
}
