use crate::db::{self, Sample};
use sqlx::SqlitePool;
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::broadcast;

pub const INTERVAL: Duration = Duration::from_secs(5);

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Collector {
    system: System,
    disks: Disks,
    pid: Pid,
    data_dir: PathBuf,
    data_mount: PathBuf,
}

impl Collector {
    pub fn new(data_dir: PathBuf) -> Self {
        let pid = sysinfo::get_current_pid().expect("current pid must be readable");
        let mut system = System::new();
        system.refresh_memory();
        system.refresh_cpu_usage();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let disks = Disks::new_with_refreshed_list();
        let data_mount = mount_point_for(&disks, &data_dir);
        Self {
            system,
            disks,
            pid,
            data_dir,
            data_mount,
        }
    }

    pub fn sample(&mut self) -> Sample {
        self.system.refresh_memory();
        self.system.refresh_cpu_usage();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        self.disks.refresh(false);

        let (disk_total, disk_used) = self
            .disks
            .list()
            .iter()
            .find(|disk| disk.mount_point() == self.data_mount)
            .map(|disk| {
                let total = disk.total_space();
                (total, total.saturating_sub(disk.available_space()))
            })
            .unwrap_or((0, 0));

        let (process_cpu_percent, process_rss) = self
            .system
            .process(self.pid)
            .map(|p| (f64::from(p.cpu_usage()), p.memory()))
            .unwrap_or((0.0, 0));

        let (data_files, data_bytes) = dir_usage(&self.data_dir);

        Sample {
            ts: now(),
            cpu_percent: f64::from(self.system.global_cpu_usage()),
            load1: System::load_average().one,
            mem_used: self.system.used_memory() as i64,
            mem_total: self.system.total_memory() as i64,
            disk_used: disk_used as i64,
            disk_total: disk_total as i64,
            process_cpu_percent,
            process_rss: process_rss as i64,
            data_files: data_files as i64,
            data_bytes: data_bytes as i64,
        }
    }
}

/// The disk whose mount point is the longest prefix of `path`.
fn mount_point_for(disks: &Disks, path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    disks
        .list()
        .iter()
        .map(|disk| disk.mount_point())
        .filter(|mount| path.starts_with(mount))
        .max_by_key(|mount| mount.as_os_str().len())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Number of regular files and their total size directly under `dir`.
fn dir_usage(dir: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .fold((0, 0), |(files, bytes), meta| {
            (files + 1, bytes + meta.len())
        })
}

/// Samples the host every [`INTERVAL`], stores each sample, and broadcasts it to
/// SSE subscribers. Runs until the runtime shuts down.
pub async fn run(pool: SqlitePool, data_dir: PathBuf, tx: broadcast::Sender<Sample>) {
    let mut collector = Collector::new(data_dir);
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // sysinfo needs two CPU refreshes spaced apart before usage is meaningful.
    ticker.tick().await;
    let mut ticks: u64 = 0;
    loop {
        ticker.tick().await;
        let sample = collector.sample();
        if let Err(error) = db::insert_sample(&pool, &sample).await {
            eprintln!("failed to store sample: {error}");
        }
        ticks += 1;
        if ticks.is_multiple_of(720)
            && let Err(error) = db::prune_samples(&pool, sample.ts).await
        {
            eprintln!("failed to prune samples: {error}");
        }
        // Nobody listening is not an error.
        let _ = tx.send(sample);
    }
}
