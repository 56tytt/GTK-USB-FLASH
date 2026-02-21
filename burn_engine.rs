use crossbeam_channel::{bounded, Receiver, Sender};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

const BUFFER_SIZE: usize = 8 * 1024 * 1024; // 8MB
const CHANNEL_DEPTH: usize = 4;

#[derive(Debug)]
pub struct BurnConfig {
    pub iso_path: PathBuf,
    pub device_path: PathBuf,
    pub verify: bool,
}

#[derive(Debug)]
pub enum BurnEvent {
    Preparing,
    Progress {
        written: u64,
        total: u64,
        speed_mbps: f64,
    },
    Verifying {
        checked: u64,
        total: u64,
    },
    Finished,
    Cancelled,
    Error(String),
}

pub enum BurnCommand {
    Start(BurnConfig),
    Cancel,
}

pub struct BurnEngine {
    cmd_tx: Sender<BurnCommand>,
    pub event_rx: Receiver<BurnEvent>,
}

impl BurnEngine {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = bounded::<BurnCommand>(2);
        let (event_tx, event_rx) = bounded::<BurnEvent>(32);

        thread::spawn(move || {
            let cancel_flag = Arc::new(AtomicBool::new(false));

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    BurnCommand::Start(cfg) => {
                        cancel_flag.store(false, Ordering::Relaxed);
                        run_burn(cfg, &event_tx, cancel_flag.clone());
                    }
                    BurnCommand::Cancel => {
                        cancel_flag.store(true, Ordering::Relaxed);
                    }
                }
            }
        });

        Self { cmd_tx, event_rx }
    }

    pub fn start(&self, cfg: BurnConfig) {
        let _ = self.cmd_tx.send(BurnCommand::Start(cfg));
    }

    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(BurnCommand::Cancel);
    }
}

fn run_burn(cfg: BurnConfig, event_tx: &Sender<BurnEvent>, cancel_flag: Arc<AtomicBool>) {
    let _ = event_tx.send(BurnEvent::Preparing);

    let total_size = match std::fs::metadata(&cfg.iso_path) {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = event_tx.send(BurnEvent::Error(e.to_string()));
            return;
        }
    };

    let mut iso = match File::open(&cfg.iso_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = event_tx.send(BurnEvent::Error(e.to_string()));
            return;
        }
    };

    let mut device = match OpenOptions::new().write(true).open(&cfg.device_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = event_tx.send(BurnEvent::Error(e.to_string()));
            return;
        }
    };

    // hint לקרנל
    unsafe {
        libc::posix_fadvise(iso.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
    }

    let (data_tx, data_rx) = bounded::<Vec<u8>>(CHANNEL_DEPTH);

    // Reader
    let reader_cancel = cancel_flag.clone();
    let reader = thread::spawn(move || loop {
        if reader_cancel.load(Ordering::Relaxed) {
            break;
        }

        let mut buffer = vec![0u8; BUFFER_SIZE];

        let read_bytes = match iso.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        buffer.truncate(read_bytes);

        if data_tx.send(buffer).is_err() {
            break;
        }
    });

    // Writer
    let start_time = Instant::now();
    let mut written: u64 = 0;
    let mut last_progress = Instant::now();

    for chunk in data_rx {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = event_tx.send(BurnEvent::Cancelled);
            return;
        }

        if let Err(e) = device.write_all(&chunk) {
            let _ = event_tx.send(BurnEvent::Error(e.to_string()));
            return;
        }

        written += chunk.len() as u64;

        // עדכון כל ~100ms
        if last_progress.elapsed() >= Duration::from_millis(100) {
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = written as f64 / elapsed / (1024.0 * 1024.0);

            let _ = event_tx.send(BurnEvent::Progress {
                written,
                total: total_size,
                speed_mbps: speed,
            });

            last_progress = Instant::now();
        }
    }

    let _ = reader.join();

    if let Err(e) = device.sync_all() {
        let _ = event_tx.send(BurnEvent::Error(e.to_string()));
        return;
    }

    if cfg.verify {
        if !verify_image(&cfg, &event_tx, cancel_flag.clone()) {
            return;
        }
    }

    let _ = event_tx.send(BurnEvent::Finished);
}

fn verify_image(
    cfg: &BurnConfig,
    event_tx: &Sender<BurnEvent>,
    cancel_flag: Arc<AtomicBool>,
) -> bool {
    let mut iso = match File::open(&cfg.iso_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = event_tx.send(BurnEvent::Error(e.to_string()));
            return false;
        }
    };

    let mut device = match File::open(&cfg.device_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = event_tx.send(BurnEvent::Error(e.to_string()));
            return false;
        }
    };

    let total = std::fs::metadata(&cfg.iso_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let mut checked = 0u64;

    let mut buf_iso = vec![0u8; BUFFER_SIZE];
    let mut buf_dev = vec![0u8; BUFFER_SIZE];

    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            let _ = event_tx.send(BurnEvent::Cancelled);
            return false;
        }

        let n1 = match iso.read(&mut buf_iso) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = event_tx.send(BurnEvent::Error(e.to_string()));
                return false;
            }
        };

        let n2 = match device.read(&mut buf_dev[..n1]) {
            Ok(n) => n,
            Err(e) => {
                let _ = event_tx.send(BurnEvent::Error(e.to_string()));
                return false;
            }
        };

        if n1 != n2 || buf_iso[..n1] != buf_dev[..n2] {
            let _ = event_tx.send(BurnEvent::Error("Verification failed".into()));
            return false;
        }

        checked += n1 as u64;

        let _ = event_tx.send(BurnEvent::Verifying { checked, total });
    }

    true
}
