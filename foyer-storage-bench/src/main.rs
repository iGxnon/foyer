//  Copyright 2024 Foyer Project Authors
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

mod analyze;
mod export;
mod rate;
mod text;
mod utils;

use std::{
    collections::BTreeMap,
    fs::create_dir_all,
    ops::{Deref, Range},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use analyze::{analyze, monitor, Metrics};
use clap::Parser;
use export::MetricsExporter;

use foyer_memory::{EvictionConfig, LfuConfig};
use foyer_storage::{
    AdmissionPolicy, AsyncStorageExt, FsDeviceConfig, FsStoreConfig, RatedTicketAdmissionPolicy,
    RatedTicketReinsertionPolicy, ReinsertionPolicy, Result, RuntimeConfig, RuntimeStoreConfig, Storage, StorageExt,
    Store, StoreConfig,
};
use futures::future::join_all;
use itertools::Itertools;
use rand::{
    distributions::Distribution,
    rngs::{OsRng, StdRng},
    Rng, SeedableRng,
};
use rate::RateLimiter;
use serde::{Deserialize, Serialize};
use text::text;
use tokio::sync::broadcast;
use utils::{detect_fs_type, dev_stat_path, file_stat_path, iostat, FsType};

#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Args {
    /// dir for cache data
    #[arg(short, long)]
    dir: String,

    /// (MiB)
    #[arg(long, default_value_t = 1024)]
    capacity: usize,

    /// (s)
    #[arg(short, long, default_value_t = 60)]
    time: u64,

    /// (s)
    #[arg(long, default_value_t = 2)]
    report_interval: u64,

    /// Some filesystem (e.g. btrfs) can span across multiple block devices and it's hard to decide
    /// which device to moitor. Use this argument to specify which block device to monitor.
    #[arg(long, default_value = "")]
    iostat_dev: String,

    /// (MiB)
    #[arg(long, default_value_t = 0.0)]
    w_rate: f64,

    /// (MiB)
    #[arg(long, default_value_t = 0.0)]
    r_rate: f64,

    #[arg(long, default_value_t = 64 * 1024)]
    entry_size_min: usize,

    #[arg(long, default_value_t = 64 * 1024)]
    entry_size_max: usize,

    #[arg(long, default_value_t = 10000)]
    lookup_range: u64,

    /// (MiB)
    #[arg(long, default_value_t = 64)]
    region_size: usize,

    #[arg(long, default_value_t = 4)]
    flushers: usize,

    #[arg(long, default_value_t = 4)]
    reclaimers: usize,

    #[arg(long, default_value_t = 4096)]
    align: usize,

    #[arg(long, default_value_t = 16 * 1024)]
    io_size: usize,

    #[arg(long, default_value_t = 16)]
    writers: usize,

    #[arg(long, default_value_t = 16)]
    readers: usize,

    #[arg(long, default_value_t = 16)]
    recover_concurrency: usize,

    /// enable rated ticket admission policy if `ticket_insert_rate_limit` > 0
    /// (MiB/s)
    #[arg(long, default_value_t = 0)]
    ticket_insert_rate_limit: usize,

    /// enable rated ticket reinsetion policy if `ticket_reinsert_rate_limit` > 0
    /// (MiB/s)
    #[arg(long, default_value_t = 0)]
    ticket_reinsert_rate_limit: usize,

    /// `0` means equal to reclaimer count
    #[arg(long, default_value_t = 0)]
    clean_region_threshold: usize,

    /// Catalog indices sharding bits.
    #[arg(long, default_value_t = 6)]
    catalog_bits: usize,

    /// weigher to enable metrics exporter
    #[arg(long, default_value_t = false)]
    metrics: bool,

    /// use separate runtime
    #[arg(long, default_value_t = false)]
    runtime: bool,

    /// available values: "none", "zstd"
    #[arg(long, default_value = "none")]
    compression: String,

    /// Time-series operation distribution.
    ///
    /// Available values: "none", "uniform", "zipf".
    ///
    /// If "uniform" or "zipf" is used, operations will be performed in async mode.
    #[arg(long, default_value = "none")]
    distribution: String,

    /// For `--distribution zipf` only.
    #[arg(long, default_value_t = 100)]
    distribution_zipf_n: usize,

    /// For `--distribution zipf` only.
    #[arg(long, default_value_t = 0.5)]
    distribution_zipf_s: f64,
}

#[derive(Debug)]
enum TimeSeriesDistribution {
    None,
    Uniform { interval: Duration },
    Zipf { n: usize, s: f64, interval: Duration },
}

impl TimeSeriesDistribution {
    fn new(args: &Args) -> Self {
        match args.distribution.as_str() {
            "none" => TimeSeriesDistribution::None,
            "uniform" => {
                // interval = 1 / freq = 1 / (rate / size) = size / rate
                let interval =
                    ((args.entry_size_min + args.entry_size_max) >> 1) as f64 / (args.w_rate * 1024.0 * 1024.0);
                let interval = Duration::from_secs_f64(interval);
                TimeSeriesDistribution::Uniform { interval }
            }
            "zipf" => {
                // interval = 1 / freq = 1 / (rate / size) = size / rate
                let interval =
                    ((args.entry_size_min + args.entry_size_max) >> 1) as f64 / (args.w_rate * 1024.0 * 1024.0);
                let interval = Duration::from_secs_f64(interval);
                display_zipf_sample(args.distribution_zipf_n, args.distribution_zipf_s);
                TimeSeriesDistribution::Zipf {
                    n: args.distribution_zipf_n,
                    s: args.distribution_zipf_s,
                    interval,
                }
            }
            other => panic!("unsupported distribution: {}", other),
        }
    }
}

struct Context {
    w_rate: Option<f64>,
    r_rate: Option<f64>,
    counts: Vec<AtomicU64>,
    entry_size_range: Range<usize>,
    lookup_range: u64,
    time: u64,
    distribution: TimeSeriesDistribution,
    metrics: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Value {
    // https://github.com/serde-rs/bytes/issues/43
    #[serde(with = "arc_bytes")]
    inner: Arc<Vec<u8>>,
}

impl Deref for Value {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.inner.as_slice()
    }
}

mod arc_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_bytes::ByteBuf;
    use std::sync::Arc;

    pub fn serialize<S>(data: &Arc<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(data.as_slice())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let buf = ByteBuf::deserialize(deserializer)?;
        Ok(Arc::new(buf.into_vec()))
    }
}

#[cfg(feature = "tokio-console")]
fn init_logger() {
    console_subscriber::init();
}

#[cfg(feature = "trace")]
fn init_logger() {
    use opentelemetry_sdk::{
        trace::{BatchConfigBuilder, Config},
        Resource,
    };
    use opentelemetry_semantic_conventions::resource::SERVICE_NAME;
    use tracing::Level;
    use tracing_subscriber::{filter::Targets, prelude::*};

    let trace_config = Config::default().with_resource(Resource::new(vec![opentelemetry::KeyValue::new(
        SERVICE_NAME,
        "foyer-storage-bench",
    )]));
    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(1048576)
        .with_max_export_batch_size(4096)
        .with_max_concurrent_exports(4)
        .build();

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .with_trace_config(trace_config)
        .with_batch_config(batch_config)
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .unwrap();
    let opentelemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_target("foyer_storage", Level::DEBUG)
                .with_target("foyer_common", Level::DEBUG)
                .with_target("foyer_intrusive", Level::DEBUG)
                .with_target("foyer_storage_bench", Level::DEBUG),
        )
        .with(opentelemetry_layer)
        .init();
}

#[cfg(not(any(feature = "tokio-console", feature = "trace")))]
fn init_logger() {
    use tracing_subscriber::{prelude::*, EnvFilter};

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                // .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_line_number(true),
        )
        .with(EnvFilter::from_default_env())
        .init();
}

#[tokio::main]
async fn main() {
    init_logger();

    #[cfg(feature = "deadlock")]
    {
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            let deadlocks = parking_lot::deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }

            println!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                println!("Deadlock #{}", i);
                for t in threads {
                    println!("Thread Id {:#?}", t.thread_id());
                    println!("{:#?}", t.backtrace());
                }
            }
            panic!()
        });
    }

    let args = Args::parse();

    if args.metrics {
        MetricsExporter::init("0.0.0.0:19970".parse().unwrap());
    }

    println!("{:#?}", args);

    assert!(args.lookup_range > 0, "\"--lookup-range\" value must be greater than 0");

    create_dir_all(&args.dir).unwrap();

    let iostat_path = match detect_fs_type(&args.dir) {
        FsType::Tmpfs => panic!("tmpfs is not supported with benches"),
        FsType::Btrfs => {
            if args.iostat_dev.is_empty() {
                panic!("cannot decide which block device to monitor for btrfs, please specify device name with \'--iostat-dev\'")
            } else {
                dev_stat_path(&args.iostat_dev)
            }
        }
        _ => file_stat_path(&args.dir),
    };

    let metrics = Metrics::default();

    let iostat_start = iostat(&iostat_path);
    let metrics_dump_start = metrics.dump();
    let time = Instant::now();

    let eviction_config = EvictionConfig::Lfu(LfuConfig {
        window_capacity_ratio: 0.1,
        protected_capacity_ratio: 0.8,
        cmsketch_eps: 0.001,
        cmsketch_confidence: 0.9,
    });

    let device_config = FsDeviceConfig {
        dir: PathBuf::from(&args.dir),
        capacity: args.capacity * 1024 * 1024,
        file_size: args.region_size * 1024 * 1024,
        align: args.align,
        io_size: args.io_size,
    };

    let mut admissions: Vec<Arc<dyn AdmissionPolicy<Key = u64, Value = Value>>> = vec![];
    let mut reinsertions: Vec<Arc<dyn ReinsertionPolicy<Key = u64, Value = Value>>> = vec![];
    if args.ticket_insert_rate_limit > 0 {
        let rt = RatedTicketAdmissionPolicy::new(args.ticket_insert_rate_limit * 1024 * 1024);
        admissions.push(Arc::new(rt));
    }
    if args.ticket_reinsert_rate_limit > 0 {
        let rt = RatedTicketReinsertionPolicy::new(args.ticket_reinsert_rate_limit * 1024 * 1024);
        reinsertions.push(Arc::new(rt));
    }

    let clean_region_threshold = if args.clean_region_threshold == 0 {
        args.reclaimers
    } else {
        args.clean_region_threshold
    };

    let compression = args
        .compression
        .as_str()
        .try_into()
        .expect("unsupported compression algorithm");

    let config = FsStoreConfig {
        name: "".to_string(),
        eviction_config,
        device_config,
        catalog_bits: args.catalog_bits,
        admissions,
        reinsertions,
        flushers: args.flushers,
        reclaimers: args.reclaimers,
        recover_concurrency: args.recover_concurrency,
        clean_region_threshold,
        compression,
    };

    let config = if args.runtime {
        StoreConfig::RuntimeFs(RuntimeStoreConfig {
            store: config,
            runtime: RuntimeConfig {
                worker_threads: None,
                thread_name: Some("foyer".to_string()),
            },
        })
    } else {
        StoreConfig::Fs(config)
    };

    println!("{config:#?}");

    let store = Store::open(config).await.unwrap();

    let (stop_tx, _) = broadcast::channel(4096);

    let handle_monitor = tokio::spawn({
        let iostat_path = iostat_path.clone();
        let metrics = metrics.clone();

        monitor(
            iostat_path,
            Duration::from_secs(args.report_interval),
            args.time,
            metrics,
            stop_tx.subscribe(),
        )
    });

    let handle_bench = tokio::spawn(bench(args.clone(), store.clone(), metrics.clone(), stop_tx.clone()));

    let handle_signal = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        tracing::warn!("foyer-storage-bench is cancelled with CTRL-C");
        stop_tx.send(()).unwrap();
    });

    handle_bench.await.unwrap();

    let iostat_end = iostat(&iostat_path);
    let metrics_dump_end = metrics.dump();
    let analysis = analyze(
        time.elapsed(),
        &iostat_start,
        &iostat_end,
        &metrics_dump_start,
        &metrics_dump_end,
    );

    store.close().await.unwrap();

    handle_monitor.abort();
    handle_signal.abort();

    println!("\nTotal:\n{}", analysis);
}

async fn bench(args: Args, store: impl Storage<u64, Value>, metrics: Metrics, stop_tx: broadcast::Sender<()>) {
    let w_rate = if args.w_rate == 0.0 {
        None
    } else {
        Some(args.w_rate * 1024.0 * 1024.0)
    };
    let r_rate = if args.r_rate == 0.0 {
        None
    } else {
        Some(args.r_rate * 1024.0 * 1024.0)
    };

    let counts = (0..args.writers).map(|_| AtomicU64::default()).collect_vec();

    let distribution = TimeSeriesDistribution::new(&args);

    let context = Arc::new(Context {
        w_rate,
        r_rate,
        lookup_range: args.lookup_range,
        counts,
        entry_size_range: args.entry_size_min..args.entry_size_max + 1,
        time: args.time,
        distribution,
        metrics: metrics.clone(),
    });

    let w_handles = (0..args.writers)
        .map(|id| tokio::spawn(write(id as u64, store.clone(), context.clone(), stop_tx.subscribe())))
        .collect_vec();
    let r_handles = (0..args.readers)
        .map(|_| tokio::spawn(read(store.clone(), context.clone(), stop_tx.subscribe())))
        .collect_vec();

    join_all(w_handles).await;
    join_all(r_handles).await;
}

async fn write(id: u64, store: impl Storage<u64, Value>, context: Arc<Context>, mut stop: broadcast::Receiver<()>) {
    let start = Instant::now();

    let mut limiter = context.w_rate.map(RateLimiter::new);
    let step = context.counts.len() as u64;

    const K: usize = 100;
    const G: usize = 10;

    let zipf_intervals = match context.distribution {
        TimeSeriesDistribution::Zipf { n, s, interval } => {
            let histogram = gen_zipf_histogram(n, s, G, n * K);

            let loop_interval = Duration::from_secs_f64(interval.as_secs_f64() * K as f64);
            let group_cnt = K / G;
            let group_interval = interval.as_secs_f64() * group_cnt as f64;

            if id == 0 {
                println!("loop interval: {loop_interval:?}, zipf intervals: ");
            }

            let mut sum = 0;
            let intervals = histogram
                .values()
                .copied()
                .map(|ratio| {
                    let cnt = ratio * K as f64;
                    sum += cnt as usize;
                    let interval = Duration::from_secs_f64(group_interval / cnt);
                    if id == 0 {
                        println!("    [{cnt:3.0} ==> {interval:010.3?}]");
                    }
                    (sum, interval)
                })
                .collect_vec();

            Some(intervals)
        }
        _ => None,
    };

    let mut c = 0;

    loop {
        let l = Instant::now();

        match stop.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            _ => return,
        }
        if start.elapsed().as_secs() >= context.time {
            return;
        }

        let idx = id + step * c;
        let entry_size = OsRng.gen_range(context.entry_size_range.clone());
        let data = Value {
            inner: Arc::new(text(idx as usize, entry_size)),
        };

        // TODO(MrCroxx): Use `let_chains` here after it is stable.
        if let Some(limiter) = &mut limiter {
            if let Some(wait) = limiter.consume(entry_size as f64) {
                tokio::time::sleep(wait).await;
            }
        }

        let time = Instant::now();
        let ctx = context.clone();
        let callback = move |res: Result<bool>| async move {
            let inserted = res.unwrap();
            let lat = time.elapsed().as_micros() as u64;
            ctx.counts[id as usize].fetch_add(1, Ordering::Relaxed);
            if let Err(e) = ctx.metrics.insert_lats.write().record(lat) {
                tracing::error!("metrics error: {:?}, value: {}", e, lat);
            }

            if inserted {
                ctx.metrics.insert_ios.fetch_add(1, Ordering::Relaxed);
                ctx.metrics.insert_bytes.fetch_add(entry_size, Ordering::Relaxed);
            }
        };

        let elapsed = l.elapsed();

        match &context.distribution {
            TimeSeriesDistribution::None => {
                let res = store.insert(idx, data).await;
                callback(res).await;
            }
            TimeSeriesDistribution::Uniform { interval } => {
                store.insert_async_with_callback(idx, data, callback);
                tokio::time::sleep(interval.saturating_sub(elapsed)).await;
            }
            TimeSeriesDistribution::Zipf { .. } => {
                store.insert_async_with_callback(idx, data, callback);
                let intervals = zipf_intervals.as_ref().unwrap();

                let group = match intervals.binary_search_by_key(&(c as usize % K), |(sum, _)| *sum) {
                    Ok(i) => i,
                    Err(i) => i.min(G - 1),
                };

                tokio::time::sleep(intervals[group].1.saturating_sub(elapsed)).await;
            }
        }

        c += 1;
    }
}

async fn read(store: impl Storage<u64, Value>, context: Arc<Context>, mut stop: broadcast::Receiver<()>) {
    let start = Instant::now();

    let mut limiter = context.r_rate.map(RateLimiter::new);
    let step = context.counts.len() as u64;

    let mut rng = StdRng::seed_from_u64(0);

    loop {
        match stop.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            _ => return,
        }
        if start.elapsed().as_secs() >= context.time {
            return;
        }

        let w = rng.gen_range(0..step); // pick a writer to read form
        let c_max = context.counts[w as usize].load(Ordering::Relaxed);
        if c_max == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        let c = rng.gen_range(std::cmp::max(c_max, context.lookup_range) - context.lookup_range..c_max);
        let idx = w + c * step;

        let time = Instant::now();
        let res = store.lookup(&idx).await.unwrap();
        let lat = time.elapsed().as_micros() as u64;

        if let Some(buf) = res {
            let entry_size = buf.len();
            assert_eq!(text(idx as usize, entry_size), *buf);
            if let Err(e) = context.metrics.get_hit_lats.write().record(lat) {
                tracing::error!("metrics error: {:?}, value: {}", e, lat);
            }
            context.metrics.get_bytes.fetch_add(entry_size, Ordering::Relaxed);

            // TODO(MrCroxx): Use `let_chains` here after it is stable.
            if let Some(limiter) = &mut limiter {
                if let Some(wait) = limiter.consume(entry_size as f64) {
                    tokio::time::sleep(wait).await;
                }
            }
        } else {
            if let Err(e) = context.metrics.get_miss_lats.write().record(lat) {
                tracing::error!("metrics error: {:?}, value: {}", e, lat);
            }
            context.metrics.get_miss_ios.fetch_add(1, Ordering::Relaxed);
        }
        context.metrics.get_ios.fetch_add(1, Ordering::Relaxed);

        tokio::task::consume_budget().await;
    }
}

fn gen_zipf_histogram(n: usize, s: f64, groups: usize, samples: usize) -> BTreeMap<usize, f64> {
    let step = n / groups;

    let mut rng = rand::thread_rng();
    let zipf = zipf::ZipfDistribution::new(n, s).unwrap();
    let mut data: BTreeMap<usize, usize> = BTreeMap::default();
    for _ in 0..samples {
        let v = zipf.sample(&mut rng);
        let g = std::cmp::min(v / step, groups);
        *data.entry(g).or_default() += 1;
    }
    let mut histogram: BTreeMap<usize, f64> = BTreeMap::default();
    for group in 0..groups {
        histogram.insert(
            group,
            data.get(&group).copied().unwrap_or_default() as f64 / samples as f64,
        );
    }
    histogram
}

fn display_zipf_sample(n: usize, s: f64) {
    const W: usize = 100;
    const H: usize = 10;

    let samples = n * 1000;

    let histogram = gen_zipf_histogram(n, s, H, samples);

    let max = histogram.values().copied().fold(0.0, f64::max);

    println!("zipf's diagram [N = {n}][s = {s}][samples = {}]", n * 1000);

    for (g, ratio) in histogram {
        let shares = (ratio / max * W as f64) as usize;
        let bar: String = if shares != 0 {
            "=".repeat(shares)
        } else {
            ".".to_string()
        };
        println!(
            "{:3} : {:6} : {:6.3}% : {}",
            g,
            (samples as f64 * ratio) as usize,
            ratio * 100.0,
            bar
        );
    }
}
