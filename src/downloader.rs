use crate::Streamer;
use crossbeam::channel::{Receiver, Sender};
use jiff::{Timestamp, tz::TimeZone};
use log::{debug, error, info, trace, warn};
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

#[derive(Clone, Debug)]
pub enum StreamerAction {
    Heartbeat,
    Start(String),
    Stop,
    StopAll,
    Update,
}

pub struct StreamerUpdate {
    pub streamer: Streamer,
    pub action: StreamerAction,
}

pub struct DownloaderProc {
    m3u8_url: String,
    name: String,
    download_path: PathBuf,
    stdout_log_path: PathBuf,
    stderr_log_path: PathBuf,
    handle: Option<Child>,
}

fn sanitize_directory_path(dir: &str) -> Option<PathBuf> {
    let dir_meta =
        std::fs::metadata(dir).expect(&format!("could not get information on directory {dir}"));
    if !dir_meta.is_dir() {
        error!("tried to sanitize a directory path {dir} that's not a directory");
        return None;
    }
    if dir_meta.permissions().readonly() {
        error!("directory path {dir} is readonly");
        return None;
    }
    Some(dir.into())
}

impl DownloaderProc {
    fn new(url: &str, name: &str, output_dir: &str, log_dir: &str) -> DownloaderProc {
        let now = Timestamp::now()
            .to_zoned(TimeZone::system())
            .strftime("%H_%M_%S-%m_%d_%Y");

        let mut output_path = sanitize_directory_path(output_dir).unwrap();
        let mut log_path = sanitize_directory_path(log_dir).unwrap();
        let mut err_log_path = log_path.clone();

        output_path.push(format!("{}-{}.mp4", name, now));
        log_path.push(format!("{}-stdout-{}.log", name, now));
        err_log_path.push(format!("{}-stderr-{}.log", name, now));
        DownloaderProc {
            m3u8_url: String::from(url),
            name: String::from(name),
            download_path: output_path,
            stdout_log_path: log_path,
            stderr_log_path: err_log_path,
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

    fn get_size_of_download_mb(&self) -> Option<u64> {
        std::fs::metadata(&self.download_path)
            .map(|meta| meta.len() / 1_000_000)
            .inspect_err(|_| warn!("output file {:?} does not exist!", self.download_path))
            .ok()
    }

    fn start_downloading(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!(
            "{}: output file: {:?} | log file: {:?}",
            self.name, self.download_path, self.stderr_log_path
        );
        let stdout_file = File::create(&self.stdout_log_path)?;
        let stderr_file = File::create(&self.stderr_log_path)?;
        let proc = Command::new("ffmpeg")
            .arg("-i")
            .arg(&self.m3u8_url)
            .arg("-c")
            .arg("copy")
            .arg(self.download_path.as_path())
            .stdout(stdout_file)
            .stderr(stderr_file)
            .spawn()?;
        self.handle = Some(proc);
        Ok(())
    }

    fn send_sigint(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("sending SIGINT downloader {} downloader", self.name);
        if let Some(proc) = self.handle.as_mut() {
            let pid = Pid::from_raw(proc.id() as i32);
            Ok(kill(pid, Signal::SIGINT)?)
        } else {
            Err(Box::new(DownloaderError::new(
                "No Active Process for Downloading User",
            )))
        }
    }

    fn cleanup_process(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("stopping downloader for {}", self.name);
        if let Some(proc) = self.handle.as_mut() {
            let mut counter = 0;
            // After sending sigint check every 250 ms if it's alive, after 3 seconds kill it
            loop {
                thread::sleep(Duration::from_millis(250));
                if proc.try_wait().unwrap().is_none() {
                    // 12 becuase 3 seconds in chunks of 250ms is 12
                    // TODO: clean this up
                    if counter >= 12 {
                        match proc.kill() {
                            Ok(()) => {
                                warn!(
                                    "SIGINT didn't stop process so used SIGKILL for: {}",
                                    self.name
                                );
                                return Ok(());
                            }
                            Err(e) => {
                                error!(
                                    "Process couldn't be stopped by SIGINT or SIGKILL. Let OS clean it up."
                                );
                                return Err(e.into());
                            }
                        }
                    } else {
                        debug!(
                            "process hasn't been fully cleaned up yet. will wait up to 3 seconds before exiting"
                        );
                        counter += 1;
                    }
                } else {
                    debug!("stopped downloader for: {}", self.name);
                    return Ok(());
                }
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
    download_directory: &str,
    log_directory: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("starting downloader thread");
    let mut isrunning = true;
    // TODO: Create hashmap here?
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
                info!("stream for {} has finished", downloader.name);
                to_remove.push(idx);
                tx_channel.send(StreamerUpdate {
                    streamer: Streamer {
                        profile_name: downloader.name.clone(),
                        download_size_mb: downloader.get_size_of_download_mb().unwrap_or(0),
                        ..Default::default()
                    },
                    action: StreamerAction::Stop,
                })?;
            } else {
                trace!("sending update for {}", downloader.name);
                tx_channel.send(StreamerUpdate {
                    streamer: Streamer {
                        profile_name: downloader.name.clone(),
                        download_size_mb: downloader.get_size_of_download_mb().unwrap_or(0),
                        ..Default::default()
                    },
                    action: StreamerAction::Update,
                })?;
            }
        }
        for idx in to_remove {
            downloaders.remove(idx);
        }
        for msg in rx_channel.try_iter() {
            debug!("handling Message for {}.", msg.streamer.profile_name);
            match msg.action {
                StreamerAction::Start(m3u8) => {
                    let mut new_downloader = DownloaderProc::new(
                        &m3u8,
                        &msg.streamer.profile_name,
                        download_directory,
                        log_directory,
                    );
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
                        .send_sigint()?;

                    downloaders
                        .iter_mut()
                        .find(|dl| dl.name == msg.streamer.profile_name)
                        .unwrap()
                        .cleanup_process()?;
                }
                StreamerAction::StopAll => {
                    // When this happens it seems borderline random whether or not there's any "known about" running streams. I have no idea what's happening to the downloaders vector. But the processes in them are successfully exiting.
                    // Actually what's probably happening is the child process is somehow consuming the control-c, stopping, then getting automatically removed from the vector. Don't know how that's happening
                    warn!(
                        "Stop All Command received. Stopping {} stream(s) and exiting.",
                        downloaders.len()
                    );
                    isrunning = false;
                    downloaders
                        .iter_mut()
                        .for_each(|dl| dl.send_sigint().unwrap());
                    downloaders
                        .iter_mut()
                        .for_each(|dl| dl.cleanup_process().unwrap());
                    downloaders.clear();
                    break;
                }
                _ => {
                    error!("heartbeat received erroneously");
                }
            }
            debug!("handled user: {}", msg.streamer.profile_name);
        }
        if isrunning {
            thread::sleep(Duration::from_secs(2));
        }
    }
    Ok(())
}
