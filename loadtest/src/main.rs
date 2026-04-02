use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use reqwest::Client;

#[derive(Parser)]
#[command(name = "pensyve-loadtest")]
struct Args {
    /// Base URL of the MCP gateway
    #[arg(long, default_value = "https://mcp.pensyve.com")]
    url: String,

    /// Bearer token (API key or JWT)
    #[arg(long, env = "PENSYVE_TOKEN")]
    token: String,

    /// Number of concurrent workers
    #[arg(short, long, default_value = "10")]
    concurrency: usize,

    /// Total number of requests
    #[arg(short, long, default_value = "100")]
    requests: usize,

    /// Test mode: recall, remember, mixed
    #[arg(short, long, default_value = "recall")]
    mode: String,
}

struct Stats {
    success: AtomicU64,
    failure: AtomicU64,
    latencies: tokio::sync::Mutex<Vec<Duration>>,
}

impl Stats {
    fn new() -> Self {
        Self {
            success: AtomicU64::new(0),
            failure: AtomicU64::new(0),
            latencies: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn record(&self, dur: Duration, ok: bool) {
        if ok {
            self.success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failure.fetch_add(1, Ordering::Relaxed);
        }
        self.latencies.lock().await.push(dur);
    }

    async fn report(&self) {
        let mut lats = self.latencies.lock().await;
        lats.sort();
        let total = lats.len();
        if total == 0 {
            println!("No requests completed.");
            return;
        }
        let sum: Duration = lats.iter().sum();
        let avg = sum / total as u32;
        let p50 = lats[total / 2];
        let p95 = lats[(total as f64 * 0.95) as usize];
        let p99 = lats[(total as f64 * 0.99).min((total - 1) as f64) as usize];
        let min = lats[0];
        let max = lats[total - 1];

        println!("\n╔══════════════════════════════════════════╗");
        println!("║          LOAD TEST RESULTS               ║");
        println!("╠══════════════════════════════════════════╣");
        println!("║  Total:     {:>6} requests               ║", total);
        println!(
            "║  Success:   {:>6}                        ║",
            self.success.load(Ordering::Relaxed)
        );
        println!(
            "║  Failed:    {:>6}                        ║",
            self.failure.load(Ordering::Relaxed)
        );
        println!(
            "║  Throughput:{:>6.1} req/s                  ║",
            total as f64 / sum.as_secs_f64() * (total as f64).min(1.0)
        );
        println!("╠══════════════════════════════════════════╣");
        println!("║  Latency (ms):                           ║");
        println!(
            "║    min:  {:>8.1}                        ║",
            min.as_secs_f64() * 1000.0
        );
        println!(
            "║    avg:  {:>8.1}                        ║",
            avg.as_secs_f64() * 1000.0
        );
        println!(
            "║    p50:  {:>8.1}                        ║",
            p50.as_secs_f64() * 1000.0
        );
        println!(
            "║    p95:  {:>8.1}                        ║",
            p95.as_secs_f64() * 1000.0
        );
        println!(
            "║    p99:  {:>8.1}                        ║",
            p99.as_secs_f64() * 1000.0
        );
        println!(
            "║    max:  {:>8.1}                        ║",
            max.as_secs_f64() * 1000.0
        );
        println!("╚══════════════════════════════════════════╝");
    }
}

async fn do_recall(client: &Client, url: &str, token: &str) -> Result<(), String> {
    let queries = [
        "recent decisions and patterns",
        "authentication flow design",
        "database migration strategy",
        "deployment pipeline status",
        "performance optimization results",
        "error handling patterns",
        "API design conventions",
        "testing strategy and coverage",
    ];
    let query = queries[rand_idx(queries.len())];

    let resp = client
        .post(format!("{url}/v1/recall"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "query": query,
            "limit": 5
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "HTTP {} — {}",
            status,
            &body[..body.len().min(200)]
        ))
    }
}

async fn do_remember(client: &Client, url: &str, token: &str, i: u64) -> Result<(), String> {
    let resp = client
        .post(format!("{url}/v1/remember"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "entity": "loadtest",
            "fact": format!("loadtest-fact-{i} this is a test memory for load testing iteration {i}"),
            "confidence": 0.5
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.status().is_success() || resp.status().as_u16() == 201 {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "HTTP {} — {}",
            status,
            &body[..body.len().min(300)]
        ))
    }
}

fn rand_idx(max: usize) -> usize {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as usize;
    t % max
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("pensyve-loadtest/0.1")
        .build()
        .expect("HTTP client");

    let stats = Arc::new(Stats::new());
    let sem = Arc::new(tokio::sync::Semaphore::new(args.concurrency));
    let counter = Arc::new(AtomicU64::new(0));

    println!(
        "Load testing {} with {} concurrency, {} total requests, mode={}",
        args.url, args.concurrency, args.requests, args.mode
    );

    let wall_start = Instant::now();
    let mut handles = Vec::new();

    for _ in 0..args.requests {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let stats = stats.clone();
        let url = args.url.clone();
        let token = args.token.clone();
        let mode = args.mode.clone();
        let i = counter.fetch_add(1, Ordering::Relaxed);

        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let result = match mode.as_str() {
                "recall" => do_recall(&client, &url, &token).await,
                "remember" => do_remember(&client, &url, &token, i).await,
                "mixed" => {
                    if i % 3 == 0 {
                        do_remember(&client, &url, &token, i).await
                    } else {
                        do_recall(&client, &url, &token).await
                    }
                }
                other => Err(format!("Unknown mode: {other}")),
            };
            let elapsed = start.elapsed();
            let ok = result.is_ok();
            if let Err(ref e) = result {
                if i < 10 {
                    eprintln!("  Request {} failed: {e}", i);
                }
            }
            stats.record(elapsed, ok).await;
            drop(permit);
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let wall_elapsed = wall_start.elapsed();
    println!(
        "\nWall clock: {:.1}s ({:.1} req/s effective)",
        wall_elapsed.as_secs_f64(),
        args.requests as f64 / wall_elapsed.as_secs_f64()
    );
    stats.report().await;

    // Cleanup: delete loadtest entity if we wrote memories
    if args.mode == "remember" || args.mode == "mixed" {
        println!("\nCleaning up loadtest entity...");
        let resp = client
            .delete(format!("{}/v1/entities/loadtest", args.url))
            .bearer_auth(&args.token)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                println!(
                    "  Cleaned up {} memories",
                    body.get("forgotten_count").unwrap_or(&serde_json::json!(0))
                );
            }
            Ok(r) => eprintln!("  Cleanup failed: HTTP {}", r.status()),
            Err(e) => eprintln!("  Cleanup failed: {e}"),
        }
    }
}
