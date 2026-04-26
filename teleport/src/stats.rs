use std::time::{Duration, Instant};

const PRINT_INTERVAL: Duration = Duration::from_secs(5);

pub struct Stats {
    name: &'static str,
    started_at: Instant,
    window_start: Instant,

    // Current 5-second window.
    updates: u64,
    bytes: u64,
    dropped: u64,
    partial_latencies: Vec<u64>,
    full_latencies: Vec<u64>,
    // source_us for partial frames; only populated on target (transit_us > 0).
    source_latencies: Vec<u64>,

    // Lifetime totals — used for the shutdown summary.
    lifetime_updates: u64,
    lifetime_bytes: u64,
    lifetime_dropped: u64,
    lifetime_latency_sum: u128,
    lifetime_latency_min: u64,
    lifetime_latency_max: u64,
}

impl Stats {
    pub fn new(name: &'static str) -> Self {
        let now = Instant::now();
        Self {
            name,
            started_at: now,
            window_start: now,
            updates: 0,
            bytes: 0,
            dropped: 0,
            partial_latencies: Vec::with_capacity(512),
            full_latencies: Vec::with_capacity(8),
            source_latencies: Vec::with_capacity(512),
            lifetime_updates: 0,
            lifetime_bytes: 0,
            lifetime_dropped: 0,
            lifetime_latency_sum: 0,
            lifetime_latency_min: u64::MAX,
            lifetime_latency_max: 0,
        }
    }

    /// Record one delivered frame.
    ///
    /// `source_us` — microseconds spent compressing on the source side
    ///   (carried in the wire header).
    /// `transit_us` — microseconds from first fragment arrival to after
    ///   decompression on the target side; pass `0` on the source.
    /// `is_full` — true for full-map frames, false for partial varBuf frames.
    pub fn record(&mut self, bytes: usize, source_us: u64, transit_us: u64, is_full: bool) {
        let total_us = source_us + transit_us;
        self.updates += 1;
        self.bytes += bytes as u64;

        if is_full {
            self.full_latencies.push(total_us);
        } else {
            self.partial_latencies.push(total_us);
            if transit_us > 0 {
                self.source_latencies.push(source_us);
            }
        }

        self.lifetime_updates += 1;
        self.lifetime_bytes += bytes as u64;
        self.lifetime_latency_sum += total_us as u128;
        if total_us < self.lifetime_latency_min {
            self.lifetime_latency_min = total_us;
        }
        if total_us > self.lifetime_latency_max {
            self.lifetime_latency_max = total_us;
        }
    }

    pub fn record_dropped(&mut self, count: u64) {
        self.dropped += count;
        self.lifetime_dropped += count;
    }

    pub fn maybe_print(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < PRINT_INTERVAL {
            return;
        }
        let elapsed_s = elapsed.as_secs_f64();
        let rate = self.updates as f64 / elapsed_s;
        let mbps = (self.bytes as f64 * 8.0) / (elapsed_s * 1_000_000.0);

        let (pp50, pp99, pmax) = percentiles(&mut self.partial_latencies);

        let full_count = self.full_latencies.len() as u64;
        let full_avg = self.full_latencies.iter().sum::<u64>().checked_div(full_count).unwrap_or(0);

        let (sp50, sp99, _) = percentiles(&mut self.source_latencies);

        let mut line = format!(
            "[{name}] {rate:.1} msg/s  {mbps:.2} Mbps  {pp50}/{pp99}/{pmax} µs p50/p99/max",
            name = self.name,
        );
        if full_count > 0 {
            line.push_str(&format!("  {full_count} full: {full_avg} µs avg"));
        }
        if !self.source_latencies.is_empty() {
            line.push_str(&format!("  src: {sp50}/{sp99} µs p50/p99"));
        }
        line.push_str(&format!("  {} dropped", self.dropped));
        println!("{line}");

        self.updates = 0;
        self.bytes = 0;
        self.dropped = 0;
        self.partial_latencies.clear();
        self.full_latencies.clear();
        self.source_latencies.clear();
        self.window_start = Instant::now();
    }

    pub fn print_summary(&self) {
        if self.lifetime_updates == 0 {
            println!("[{}] summary: no data transferred", self.name);
            return;
        }
        let dur_s = self.started_at.elapsed().as_secs_f64();
        let mb = self.lifetime_bytes as f64 / 1_000_000.0;
        let avg = (self.lifetime_latency_sum / self.lifetime_updates as u128) as u64;
        println!(
            "[{name}] summary: {dur:.0}s  {msgs} msgs  {mb:.1} MB  avg {avg} µs  min {min} µs  max {max} µs  {dropped} dropped",
            name = self.name,
            dur = dur_s,
            msgs = self.lifetime_updates,
            min = self.lifetime_latency_min,
            max = self.lifetime_latency_max,
            dropped = self.lifetime_dropped,
        );
    }
}

fn percentiles(samples: &mut [u64]) -> (u64, u64, u64) {
    if samples.is_empty() {
        return (0, 0, 0);
    }
    samples.sort_unstable();
    let n = samples.len();
    let p50 = samples[n / 2];
    let p99 = samples[(n * 99 / 100).min(n - 1)];
    let max = samples[n - 1];
    (p50, p99, max)
}
