//! An interactive terminal dashboard; systemd gets structured, non-animated logs.
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    io::IsTerminal,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static ENABLED: AtomicBool = AtomicBool::new(true);
pub fn set_enabled(value: bool) {
    ENABLED.store(value, Ordering::Relaxed);
}
pub struct Progress {
    multi: MultiProgress,
    header: ProgressBar,
    start: Instant,
    workers: usize,
    cache_mib: usize,
    message: Mutex<String>,
    last_refresh: AtomicU64,
    cgroup: Option<PathBuf>,
    last_pressure_warning: AtomicU64,
}
impl Progress {
    pub fn new(workers: usize, cache_mib: usize) -> Self {
        let visible = ENABLED.load(Ordering::Relaxed) && std::io::stderr().is_terminal();
        let multi = MultiProgress::with_draw_target(if visible {
            ProgressDrawTarget::stderr_with_hz(10)
        } else {
            ProgressDrawTarget::hidden()
        });
        let header = multi.add(ProgressBar::new_spinner());
        header.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold} {msg}").unwrap(),
        );
        header.set_prefix("VoxelAtlas");
        header.enable_steady_tick(Duration::from_millis(150));
        let result = Self {
            multi,
            header,
            start: Instant::now(),
            workers,
            cache_mib,
            message: Mutex::new(String::new()),
            last_refresh: AtomicU64::new(u64::MAX),
            cgroup: std::fs::read_to_string("/proc/self/cgroup")
                .ok()
                .and_then(|text| {
                    text.lines().find_map(|line| {
                        line.strip_prefix("0::").map(|path| {
                            PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'))
                        })
                    })
                }),
            last_pressure_warning: AtomicU64::new(u64::MAX),
        };
        result.stage("Loading assets");
        result
    }
    pub fn stage(&self, message: &str) {
        *self.message.lock().unwrap() = message.to_owned();
        self.last_refresh.store(u64::MAX, Ordering::Relaxed);
        self.refresh();
    }
    pub fn refresh(&self) {
        let elapsed = self.start.elapsed().as_secs();
        if self.last_refresh.swap(elapsed, Ordering::Relaxed) == elapsed {
            return;
        }
        let message = self.message.lock().unwrap();
        let rss = resident_mib();
        let mut budget = String::new();
        if let Some(path) = &self.cgroup {
            let read = |name: &str| std::fs::read_to_string(path.join(name)).ok();
            if let (Some(current), Some(maximum)) = (read("memory.current"), read("memory.max"))
                && let (Ok(current), Ok(maximum)) =
                    (current.trim().parse::<u64>(), maximum.trim().parse::<u64>())
            {
                budget = format!(
                    " · cgroup {}/{} MiB",
                    current / 1024 / 1024,
                    maximum / 1024 / 1024
                );
            }
            let pressure = read("memory.pressure")
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.starts_with("full "))
                        .and_then(|line| {
                            line.split_whitespace().find_map(|part| {
                                part.strip_prefix("avg10=")
                                    .and_then(|value| value.parse::<f64>().ok())
                            })
                        })
                })
                .unwrap_or(0.);
            if pressure >= 1. {
                budget.push_str(&format!(" · memory stalls {pressure:.0}%"));
            }
            let last = self.last_pressure_warning.load(Ordering::Relaxed);
            if pressure >= 10. && (last == u64::MAX || elapsed.saturating_sub(last) >= 30) {
                self.last_pressure_warning.store(elapsed, Ordering::Relaxed);
                tracing::warn!(
                    memory_stall_percent = pressure,
                    "Memory reclaim is throttling rendering; reduce the working set or adjust MemoryHigh within the hard memory limit"
                );
            }
        }
        self.header.set_message(format!(
            "{message}  | {} workers · {} MiB cache · RSS {rss} MiB{budget} · {:.0}s",
            self.workers,
            self.cache_mib,
            self.start.elapsed().as_secs_f64()
        ));
    }
    pub fn bar(&self, count: usize, label: &str) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(count as u64));
        bar.set_style(
            ProgressStyle::with_template(
                "{prefix:.bold} [{wide_bar:.cyan/blue}] {pos}/{len} · {per_sec} · stage ETA {eta}",
            )
            .unwrap()
            .progress_chars("━╸─"),
        );
        bar.set_prefix(label.to_owned());
        bar.enable_steady_tick(Duration::from_millis(250));
        bar
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        self.header.finish_and_clear();
        let _ = self.multi.clear();
    }
}
pub fn resident_mib() -> usize {
    memory_field("VmRSS:")
}
pub fn peak_mib() -> usize {
    memory_field("VmHWM:")
}
fn memory_field(field: &str) -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<usize>().ok())
        })
        .unwrap_or(0)
        / 1024
}
