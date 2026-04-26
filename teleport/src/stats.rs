use std::time::{Duration, Instant};

const PRINT_INTERVAL: Duration = Duration::from_secs(5);

pub struct Stats {
    name: &'static str,
    started_at: Instant,
    window_start: Instant,

    // Current 5-second window.
    updates: u64,
    bytes: u64,
    fragments: u64,
    dropped: u64,
    latencies: Vec<u64>,

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
            fragments: 0,
            dropped: 0,
            latencies: Vec::with_capacity(512),
            lifetime_updates: 0,
            lifetime_bytes: 0,
            lifetime_dropped: 0,
            lifetime_latency_sum: 0,
            lifetime_latency_min: u64::MAX,
            lifetime_latency_max: 0,
        }
    }

    pub fn record(&mut self, bytes: usize, fragments: u16, latency_us: u64) {
        self.updates += 1;
        self.bytes += bytes as u64;
        self.fragments += fragments as u64;
        self.latencies.push(latency_us);

        self.lifetime_updates += 1;
        self.lifetime_bytes += bytes as u64;
        self.lifetime_latency_sum += latency_us as u128;
        if latency_us < self.lifetime_latency_min {
            self.lifetime_latency_min = latency_us;
        }
        if latency_us > self.lifetime_latency_max {
            self.lifetime_latency_max = latency_us;
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
        let avg_frags = self.fragments as f64 / self.updates.max(1) as f64;
        let (p50, p99, max) = percentiles(&mut self.latencies);

        println!(
            "[{name}] {rate:.1} msg/s  {mbps:.2} Mbps  {frags:.1} frags/msg  {p50}/{p99}/{max} µs p50/p99/max  {dropped} dropped",
            name = self.name,
            frags = avg_frags,
            dropped = self.dropped,
        );

        self.updates = 0;
        self.bytes = 0;
        self.fragments = 0;
        self.dropped = 0;
        self.latencies.clear();
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
            mb = mb,
            avg = avg,
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
