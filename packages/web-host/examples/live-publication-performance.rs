//! Bounded black-box retained-publication evidence; no production instrumentation.
//!
//! Coordinator invocation (selected assets must already exist):
//! cargo run -p bmux_web_host --release --features presentation \
//!   --example live-publication-performance -- /absolute/assets 128
//! Optional browser driver mode: ... -- /absolute/assets --serve
//! The driver sends bounded `steady|change|image-steady|image-change|reset|quit`
//! lines on stdin. This example never starts an application/user runtime.
//!
//! Allocator scopes include only synchronous encoding or publication on this thread.
//! alloc/realloc bytes sum successful requested sizes (realloc counts its NEW size).
//! Live/peak bytes count all outstanding System allocator requested sizes, including
//! source, feed, host assets and measurement scaffolding; NOT RSS or allocator arenas.
//! The single-thread host runtime is paused during these scopes, so serving does not
//! contaminate them. Retained checkpoints are quiescent process heap observations,
//! not estimates of private publisher state. HTTP/decode work is outside scopes.
//! Publication latency ends at successful feed commit, NOT delivery or browser paint.

#[cfg(not(feature = "presentation"))]
fn main() {
    eprintln!("live-publication-performance requires --features presentation");
    std::process::exit(2);
}

#[cfg(feature = "presentation")]
fn main() -> anyhow::Result<()> {
    harness::run()
}

#[cfg(feature = "presentation")]
mod harness {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, ensure};
    use bmux_tui::damage::Damage;
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{
        ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
        ImagePlacement,
    };
    use bmux_tui::presentation::{PreparedPresentation, PresentationState};
    use bmux_tui::style::Style;
    use bmux_web_host::{BrowserPublisher, HostConfig, HostHandle, snapshot_feed};
    use bmux_web_renderer::{BrowserFrame, wire};
    use serde_json::json;
    use tokio::runtime::Runtime;

    const GRID: Rect = Rect::new(0, 0, 80, 24);
    const MAX_SAMPLES: usize = 1024;
    const MAX_BODY: u64 = 16 * 1024 * 1024;
    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Copy, Default)]
    struct Allocations {
        calls: usize,
        bytes: usize,
    }
    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static COUNTS: Cell<Allocations> = const { Cell::new(Allocations { calls: 0, bytes: 0 }) };
    }

    struct CountingAllocator;
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    fn allocated(size: usize) {
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(live, Ordering::Relaxed);
        let _ = ACTIVE.try_with(|active| {
            if active.get() {
                let _ = COUNTS.try_with(|counts| {
                    let old = counts.get();
                    counts.set(Allocations {
                        calls: old.calls + 1,
                        bytes: old.bytes + size,
                    });
                });
            }
        });
    }

    // SAFETY: every operation forwards the identical pointer/layout contract to System.
    // Accounting allocates nothing and records only successful allocation requests.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: caller supplies the GlobalAlloc layout contract.
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                allocated(layout.size());
            }
            pointer
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: caller supplies the GlobalAlloc layout contract.
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                allocated(layout.size());
            }
            pointer
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            // SAFETY: pointer/layout are passed through unchanged.
            unsafe {
                System.dealloc(pointer, layout);
            }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            // SAFETY: pointer/layout/new size are passed through unchanged.
            let replacement = unsafe { System.realloc(pointer, layout, size) };
            if !replacement.is_null() {
                LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
                allocated(size);
            }
            replacement
        }
    }

    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(false));
        }
    }
    #[derive(Clone, Copy)]
    struct Sample {
        ns: u128,
        allocations: Allocations,
        live_before: usize,
        live_after: usize,
        peak: usize,
    }
    fn measure<T>(work: impl FnOnce() -> T) -> (T, Sample) {
        assert!(!ACTIVE.with(Cell::get), "nested allocation scope");
        COUNTS.with(|counts| counts.set(Allocations::default()));
        let before = LIVE.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        ACTIVE.with(|active| active.set(true));
        let guard = Scope;
        let start = Instant::now();
        let value = std::hint::black_box(work());
        let elapsed = start.elapsed().as_nanos();
        drop(guard);
        (
            value,
            Sample {
                ns: elapsed,
                allocations: COUNTS.with(Cell::get),
                live_before: before,
                live_after: LIVE.load(Ordering::Relaxed),
                peak: PEAK.load(Ordering::Relaxed),
            },
        )
    }
    fn sample_json(sample: Sample) -> serde_json::Value {
        json!({"ns": sample.ns, "allocation_calls": sample.allocations.calls,
            "allocated_requested_bytes": sample.allocations.bytes,
            "live_requested_bytes_before": sample.live_before,
            "live_requested_bytes_after": sample.live_after,
            "publication_net_live_requested_bytes": i128::try_from(sample.live_after).expect("heap byte count fits i128")
                - i128::try_from(sample.live_before).expect("heap byte count fits i128"),
            "peak_live_requested_bytes": sample.peak})
    }
    fn summary(samples: &[Sample]) -> serde_json::Value {
        fn percent(values: &mut [u128], numerator: usize) -> u128 {
            values.sort_unstable();
            values[(values.len() * numerator).div_ceil(100) - 1]
        }
        let mut times: Vec<_> = samples.iter().map(|s| s.ns).collect();
        let p50 = percent(&mut times, 50);
        let p95 = percent(&mut times, 95);
        json!({"count": samples.len(), "ns_p50": p50, "ns_p95": p95,
            "allocation_calls_total": samples.iter().map(|s| s.allocations.calls).sum::<usize>(),
            "allocated_requested_bytes_total": samples.iter().map(|s| s.allocations.bytes).sum::<usize>(),
            "live_requested_bytes_first": samples[0].live_after,
            "live_requested_bytes_middle": samples[samples.len()/2].live_after,
            "live_requested_bytes_last": samples[samples.len()-1].live_after,
            "reuse_live_requested_bytes_min": samples.iter().map(|s| s.live_after).min(),
            "reuse_live_requested_bytes_max": samples.iter().map(|s| s.live_after).max(),
            "peak_live_requested_bytes": samples.iter().map(|s| s.peak).max()})
    }

    fn prepared(image: bool, changed: bool) -> Result<Arc<PreparedPresentation>> {
        let state = PresentationState::new(GRID);
        let output = state
            .prepare(Damage::Full, |paint| {
                for row in 0_i64..24 {
                    for col in 0_i32..80 {
                        let symbol = if (row + i64::from(col)) % 3 == 0 {
                            "M"
                        } else {
                            "."
                        };
                        paint.set_cell(col, row, symbol, Style::default());
                    }
                }
                paint.set_cell(0, 0, if changed { "B" } else { "A" }, Style::default());
                if image {
                    paint.push_image(ImageContribution::Present(ImagePlacement {
                        key: ImageKey::new("performance-owner/image"),
                        payload: ImagePayload::Pixels {
                            bytes: vec![if changed { 192 } else { 64 }; 256 * 256 * 4],
                            width: 256,
                            height: 256,
                            format: ImagePixelFormat::Rgba8,
                        },
                        destination: Rect::new(4, 4, 16, 8),
                        clip: GRID,
                        fit: ImageFit::Contain,
                        allocation: None,
                        lifecycle: ImageLifecycle::Frame,
                    }));
                }
            })
            .context("prepared measurement scene has no damage")?;
        Ok(Arc::new(output))
    }

    struct Fixture {
        runtime: Runtime,
        host: HostHandle,
        publisher: BrowserPublisher,
        client: reqwest::blocking::Client,
    }
    impl Fixture {
        fn new(assets: &std::path::Path) -> Result<Self> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let (writer, feed) = snapshot_feed(16 * 1024 * 1024)?;
            let mut config = HostConfig::new(assets.to_owned());
            config.port = 0;
            let host = runtime.block_on(bmux_web_host::start(config, feed))?;
            let publisher = BrowserPublisher::new(writer, host.view_control());
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()?;
            Ok(Self {
                runtime,
                host,
                publisher,
                client,
            })
        }
        fn fetch(&self, path: &str) -> Result<Vec<u8>> {
            let client = self.client.clone();
            let url = format!("http://{}{}", self.host.address(), path);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let worker = std::thread::spawn(move || {
                let result = (|| -> Result<Vec<u8>> {
                    let response = client.get(url).send()?.error_for_status()?;
                    let mut bytes = Vec::new();
                    response.take(MAX_BODY + 1).read_to_end(&mut bytes)?;
                    ensure!(bytes.len() as u64 <= MAX_BODY, "HTTP body bound exceeded");
                    Ok(bytes)
                })();
                let _ = sender.send(result);
            });
            // Serving is driven only here, never concurrently with native measurements.
            let result = self.runtime.block_on(receiver)?;
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("HTTP helper panicked"))?;
            result
        }
        fn decode(&self, bytes: &[u8], base: Option<&BrowserFrame>) -> Result<BrowserFrame> {
            let mut resources = Vec::new();
            for requirement in wire::resource_requirements(bytes)? {
                let data = self.fetch(&format!("/resource/{}", requirement.id))?;
                resources.push(requirement.with_bytes(data)?);
            }
            Ok(wire::decode_binary_with_resources(bytes, base, &resources)?)
        }
    }

    fn encoding(
        source: &PreparedPresentation,
        changed: &PreparedPresentation,
        count: usize,
        change: bool,
    ) -> Result<()> {
        let base = BrowserFrame::from_prepared_in_generation("encoding", 1, source)?;
        let next = BrowserFrame::from_prepared_in_generation("encoding", 2, changed)?;
        let mut full_samples = Vec::with_capacity(count);
        let mut delta_samples = Vec::with_capacity(count);
        let mut full_bytes = 0;
        let mut delta_bytes = 0;
        for _ in 0..count {
            let (full, full_sample) = measure(|| wire::encode_binary_with_resources(&next));
            full_bytes = full?.len();
            full_samples.push(full_sample);
            let (delta, delta_sample) = measure(|| wire::encode_delta_with_resources(&next, &base));
            delta_bytes = delta?.len();
            delta_samples.push(delta_sample);
        }
        println!(
            "{}",
            json!({"metric": "encoding_probe", "has_image": source.image_scene().placements().len() != 0,
            "change": change, "full_bytes": full_bytes, "delta_bytes": delta_bytes,
            "full_first": sample_json(full_samples[0]), "delta_first": sample_json(delta_samples[0]),
            "full_reuse": summary(&full_samples[1..]), "delta_reuse": summary(&delta_samples[1..])})
        );
        Ok(())
    }

    fn workload(
        assets: &std::path::Path,
        samples: usize,
        image: bool,
        change: bool,
        slow: bool,
    ) -> Result<()> {
        let sources = [prepared(image, false)?, prepared(image, true)?];
        let held_sources = sources.clone();
        let baseline_live = LIVE.load(Ordering::Relaxed);
        let mut fixture = Fixture::new(assets)?;
        let mut reused = Vec::with_capacity(samples);
        let (revision, first) = measure(|| {
            fixture
                .publisher
                .publish_prepared("performance-owner", 1, &sources[0])
        });
        ensure!(revision? == 1, "first transport identity");
        // A slow observer retains its old decoded visual but makes no reads during the burst.
        let initial_bytes = fixture.fetch("/presentation?version=1")?;
        let initial = fixture.decode(&initial_bytes, None)?;
        let initial_expected =
            BrowserFrame::from_prepared_in_generation("performance-owner", 1, &sources[0])?;
        ensure!(
            initial == initial_expected,
            "first feed bytes differ from prepared source"
        );
        let mut previous = initial.clone();
        let mut delivered_bytes = 0;
        for index in 0..samples {
            let source = &sources[usize::from(change && index % 2 == 0)];
            let source_revision = u64::try_from(index)? + 2;
            let (revision, sample) = measure(|| {
                fixture
                    .publisher
                    .publish_prepared("performance-owner", source_revision, source)
            });
            ensure!(revision? == source_revision, "monotonic transport identity");
            reused.push(sample);
            if !slow {
                let bytes = fixture.fetch(&format!(
                    "/presentation?version=1&base={}&after={}",
                    source_revision - 1,
                    source_revision - 1
                ))?;
                delivered_bytes += bytes.len();
                previous = fixture.decode(&bytes, Some(&previous))?;
                ensure!(
                    previous.revision() == source_revision,
                    "reader did not receive latest revision"
                );
            }
        }
        let latest_revision = u64::try_from(samples)? + 1;
        let full = fixture.fetch("/presentation?version=1")?;
        let latest = fixture.decode(&full, None)?;
        ensure!(
            latest.revision() == latest_revision,
            "latest-only full repair lost publication"
        );
        // Exact predecessor must select a delta for this sparse cell workload.
        let bytes = fixture.fetch(&format!(
            "/presentation?version=1&base={}",
            latest_revision - 1
        ))?;
        let predecessor_source = &sources[usize::from(change && (samples - 2).is_multiple_of(2))];
        let predecessor = BrowserFrame::from_prepared_in_generation(
            "performance-owner",
            latest_revision - 1,
            predecessor_source,
        )?;
        ensure!(
            fixture.decode(&bytes, Some(&predecessor))? == latest,
            "exact-predecessor delta did not reconstruct latest visual"
        );
        let latest_resources = wire::extract_resources(&latest)?;
        ensure!(
            wire::decode_binary_with_resources(&bytes, None, &latest_resources).is_err(),
            "expected exact-predecessor delta"
        );
        ensure!(
            wire::decode_binary_with_resources(&bytes, Some(&initial), &latest_resources).is_err(),
            "stale delta base accepted"
        );
        // A stale transport cursor must instead receive independently decodable full bytes.
        let repair = fixture.fetch("/presentation?version=1&base=1&after=1")?;
        ensure!(
            repair == full,
            "missed-base repair did not choose latest full bytes"
        );
        ensure!(
            fixture.decode(&repair, None)? == latest,
            "full repair differs from latest complete visual"
        );
        ensure!(
            initial == initial_expected,
            "retained old observer snapshot was mutated"
        );
        for (source, held) in sources.iter().zip(&held_sources) {
            ensure!(
                Arc::ptr_eq(source, held),
                "prepared source stopped sharing immutable ownership"
            );
        }
        ensure!(
            BrowserFrame::from_prepared_in_generation("performance-owner", 1, &held_sources[0])?
                == initial_expected,
            "long-lived prepared owner snapshot changed during publication"
        );
        println!(
            "{}",
            json!({"metric": "publication", "grid": [80,24], "image_rgba_bytes": if image {256*256*4} else {0},
            "image": image, "change": change, "consumer": if slow {"slow_no_reads_during_burst"} else {"read_each_update"},
            "baseline_live_requested_bytes_including_sources_and_scaffolding": baseline_live,
            "first_admission": sample_json(first), "reuse": summary(&reused),
            "full_bytes": full.len(), "exact_delta_bytes": bytes.len(), "read_each_wire_bytes": delivered_bytes,
            "slow_repair_skipped_publications": if slow {samples-1} else {0},
            "retained_old_observer_revision": initial.revision(), "latest_revision": latest_revision})
        );
        fixture
            .publisher
            .publish_prepared("replacement-owner", 1, &sources[0])?;
        let generation_before_drop =
            fixture.fetch(&format!("/presentation?version=1&base={latest_revision}"))?;
        ensure!(
            fixture.decode(&generation_before_drop, None)?.generation() == "replacement-owner",
            "generation reset needs full repair"
        );
        let before_drop = LIVE.load(Ordering::Relaxed);
        let Fixture {
            runtime,
            host,
            publisher,
            client,
        } = fixture;
        drop(publisher);
        runtime.block_on(host.shutdown());
        drop(client);
        drop(runtime);
        let after_drop = LIVE.load(Ordering::Relaxed);
        println!(
            "{}",
            json!({"metric": "owned_fixture_release", "image": image, "change": change, "slow": slow,
            "live_requested_bytes_before_drop": before_drop, "live_requested_bytes_after_drop": after_drop,
            "fixture_release_requested_bytes": before_drop.saturating_sub(after_drop),
            "definition": "actual dropped publisher/feed/host/client/runtime heap requests; observer/source/probe allocations still retained; not RSS"})
        );
        Ok(())
    }

    fn monotonic_ns() -> Result<u128> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: time is writable and CLOCK_MONOTONIC has no external side effects.
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut time) } == 0,
            "CLOCK_MONOTONIC unavailable"
        );
        Ok(u128::try_from(time.tv_sec)? * 1_000_000_000 + u128::try_from(time.tv_nsec)?)
    }

    fn serve(assets: &std::path::Path) -> Result<()> {
        let sources = [
            prepared(false, false)?,
            prepared(false, true)?,
            prepared(true, false)?,
            prepared(true, true)?,
        ];
        let mut fixture = Fixture::new(assets)?;
        fixture
            .publisher
            .publish_prepared("browser-performance", 1, &sources[0])?;
        println!(
            "{}",
            json!({"ready": fixture.host.bootstrap_url(), "revision": 1})
        );
        std::io::stdout().flush()?;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let reader = std::thread::spawn(move || {
            let input = std::io::stdin();
            let mut input = input.lock();
            loop {
                let mut line = String::new();
                // Bound command input even before newline; no unbounded lines()/read_line().
                let read = std::io::BufRead::read_line(&mut (&mut input).take(65), &mut line);
                match read {
                    Err(_) => break,
                    Ok(0) => break,
                    Ok(_) if line.len() > 64 => break,
                    Ok(_) => {
                        if sender.blocking_send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut revision = 1;
        for _ in 0..MAX_SAMPLES {
            let command = fixture
                .runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(30), receiver.recv()).await
                })?
                .context("driver closed")?;
            if command.trim() == "quit" {
                break;
            }
            let index = match command.trim() {
                "steady" | "reset" => 0,
                "change" => 1,
                "image-steady" => 2,
                "image-change" => 3,
                _ => anyhow::bail!("unsupported bounded driver command"),
            };
            revision += 1;
            let generation = if command.trim() == "reset" {
                "browser-replacement"
            } else {
                "browser-performance"
            };
            let source_monotonic_ns = monotonic_ns()?;
            fixture
                .publisher
                .publish_prepared(generation, revision, &sources[index])?;
            println!(
                "{}",
                json!({"published_source_revision": revision, "source_monotonic_ns": source_monotonic_ns})
            );
            std::io::stdout().flush()?;
        }
        fixture.runtime.block_on(fixture.host.shutdown());
        // Closing stdin is the driver's responsibility; do not wait on a blocked reader.
        drop(reader);
        Ok(())
    }

    pub fn run() -> Result<()> {
        // Match the CLI/resolver provider selection before either native or --serve
        // mode constructs a reqwest client. An already-installed provider is retained.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut args = std::env::args_os().skip(1);
        let assets =
            PathBuf::from(args.next().context(
                "usage: live-publication-performance /absolute/assets [2..1024|--serve]",
            )?);
        ensure!(
            assets.is_absolute(),
            "assets must be explicitly selected absolute path"
        );
        let mode = args.next().unwrap_or_else(|| "128".into());
        ensure!(args.next().is_none(), "unexpected arguments");
        if mode == "--serve" {
            return serve(&assets);
        }
        let samples: usize = mode.to_str().context("invalid sample count")?.parse()?;
        ensure!(
            (2..=MAX_SAMPLES).contains(&samples),
            "sample count must be 2..1024"
        );
        println!(
            "{}",
            json!({"schema": "bmux.live-publication-performance.v1", "samples_per_case": samples,
            "percentiles": "nearest rank", "memory": "System allocator live requested heap bytes, not RSS",
            "latency": "synchronous prepared projection/encoding/feed commit, not browser paint",
            "allocator": "System requested sizes; successful realloc counts one call and new size; deallocation updates live only",
            "owned_source": "two long-lived immutable Arc<PreparedPresentation> variants, cloned ownership retained across the burst",
            "allocation_scope": "calling thread only; preparation, HTTP, decoding, printing excluded"})
        );
        for image in [false, true] {
            let source = prepared(image, false)?;
            let changed = prepared(image, true)?;
            encoding(&source, &source, samples, false)?;
            encoding(&source, &changed, samples, true)?;
            for change in [false, true] {
                for slow in [false, true] {
                    workload(&assets, samples, image, change, slow)?;
                }
            }
        }
        Ok(())
    }
}
