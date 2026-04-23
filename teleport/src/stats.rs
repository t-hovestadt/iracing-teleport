use std::time::{Duration, Instant};

const PRINT_INTERVAL: Duration = Duration::from_secs(5);

pub struct Stats {
    name: &'static str,
    window_start: Instant,
    updates: u64,
    bytes: u64,
    fragments: u64,
    latency_us: u64,
}

impl Stats {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            window_start: Instant::now(),
            updates: 0,
            bytes: 0,
            fragments: 0,
            latency_us: 0,
        }
    }

    pub fn record(&mut self, bytes: usize, fragments: u16, latency_us: u64) {
        self.updates += 1;
        self.bytes += bytes as u64;
        self.fragments += fragments as u64;
        self.latency_us += latency_us;
    }

    pub fn maybe_print(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < PRINT_INTERVAL {
            return;
        }
        let elapsed = elapsed.as_secs_f64();
        let rate = self.updates as f64 / elapsed;
        let mbps = (self.bytes as f64 * 8.0) / (elapsed * 1_000_000.0);
        let avg_frags = self.fragments as f64 / self.updates.max(1) as f64;
        let avg_lat = self.latency_us as f64 / self.updates.max(1) as f64;

        println!(
            "[{name}] {rate:.1} msg/s  {mbps:.2} Mbps  {frags:.1} frags/msg  {lat:.0} µs avg latency",
            name = self.name,
            rate = rate,
            mbps = mbps,
            frags = avg_frags,
            lat = avg_lat,
        );

        self.updates = 0;
        self.bytes = 0;
        self.fragments = 0;
        self.latency_us = 0;
        self.window_start = Instant::now();
    }
}
