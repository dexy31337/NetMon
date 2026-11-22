use crate::{
    collector::{self, Collector},
    engine::Engine,
    model::*,
    storage::Store,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct CaptureTask {
    pub shared: Arc<RwLock<Arc<Snapshot>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl CaptureTask {
    pub fn start(config: CaptureConfig, dir: PathBuf) -> Self {
        let shared = Arc::new(RwLock::new(Arc::new(Snapshot::default())));
        let stop = Arc::new(AtomicBool::new(false));
        let target = shared.clone();
        let stop_worker = stop.clone();
        let thread = thread::spawn(move || {
            let mut e = Engine::new(config, collector::processes());
            let result = run(&mut e, &dir, &stop_worker, &target);
            e.finish();
            if let Err(err) = result {
                e.snapshot.error = Some(format!("{err:#}"));
            }
            if let Ok(mut store) = Store::open(&dir)
                && let Err(err) = store.save(&mut e)
            {
                e.snapshot.error = Some(format!("Ошибка сохранения: {err:#}"));
            }
            *target.write().unwrap() = Arc::new(e.snapshot);
        });
        Self {
            shared,
            stop,
            thread: Some(thread),
        }
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.shared.read().unwrap().clone()
    }
}
impl Drop for CaptureTask {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    e: &mut Engine,
    dir: &std::path::Path,
    stop: &AtomicBool,
    shared: &RwLock<Arc<Snapshot>>,
) -> anyhow::Result<()> {
    let mut store = Store::open(dir)?;
    let (tx, rx) = crossbeam_channel::bounded(131_072);
    let mut collector = Collector::start(tx)?;
    // Refresh after enabling providers to close the process-start/snapshot race.
    for p in collector::processes() {
        e.register(p);
    }
    store.save(e)?;
    *shared.write().unwrap() = Arc::new(e.snapshot.clone());
    let mut pending = Vec::new();
    let mut last_publish = Instant::now();
    let mut persistence_error = None;
    while !stop.load(Ordering::Relaxed) {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
            pending.push(event);
        }
        pending.extend(rx.try_iter().take(16_384));
        if pending.len() > 262_144 {
            persistence_error = Some(anyhow::anyhow!(
                "Поток событий превысил лимит буфера; запись остановлена."
            ));
            break;
        }
        if last_publish.elapsed() >= Duration::from_millis(700) {
            // ETW sessions flush independently; reorder by event time before correlation.
            pending.sort_by_key(|event| event.ticks);
            let limit = now_ms() - 1800;
            let count =
                pending.partition_point(|event| event.ticks / 10_000 - 11_644_473_600_000 <= limit);
            for event in pending.drain(..count) {
                e.apply(event.event);
            }
            e.tick(limit);
            collector.update_health(&mut e.snapshot.recording.as_mut().unwrap().health);
            if e.snapshot.flows.len() + e.snapshot.dns.len() >= 50_000 {
                e.snapshot.recording.as_mut().unwrap().health.messages.push(
                    "Достигнут лимит 50 000 строк одной записи. Начните новую запись.".into(),
                );
                break;
            }
            if let Err(err) = store.save(e) {
                persistence_error = Some(err);
                break;
            }
            *shared.write().unwrap() = Arc::new(e.snapshot.clone());
            last_publish = Instant::now();
        }
    }
    collector.stop(&mut e.snapshot.recording.as_mut().unwrap().health);
    pending.extend(rx.try_iter());
    pending.sort_by_key(|event| event.ticks);
    for event in pending {
        e.apply(event.event);
    }
    e.finish();
    store.save(e)?;
    if let Some(err) = persistence_error {
        return Err(err);
    }
    Ok(())
}
