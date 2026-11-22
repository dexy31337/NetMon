use crate::{
    collector::{self, Collector},
    engine::Engine,
    model::*,
    storage::Store,
};
use anyhow::Context;
use std::{
    path::{Path, PathBuf},
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
        Self::spawn(config, Some(dir))
    }

    pub fn start_live(config: CaptureConfig) -> Self {
        Self::spawn(config, None)
    }

    fn spawn(config: CaptureConfig, history_dir: Option<PathBuf>) -> Self {
        let shared = Arc::new(RwLock::new(Arc::new(Snapshot::default())));
        let stop = Arc::new(AtomicBool::new(false));
        let target = shared.clone();
        let stop_worker = stop.clone();
        let thread = thread::spawn(move || {
            let engine = Engine::new(config, collector::processes());
            let snapshot = capture(engine, history_dir.as_deref(), |engine, store| {
                run(engine, store, &stop_worker, &target)
            });
            *target.write().unwrap() = Arc::new(snapshot);
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

fn capture(
    mut engine: Engine,
    history_dir: Option<&Path>,
    collect: impl FnOnce(&mut Engine, Option<&mut Store>) -> anyhow::Result<()>,
) -> Snapshot {
    let mut store = history_dir.map(Store::open).transpose();
    let error = match &mut store {
        Ok(store) => collect(&mut engine, store.as_mut())
            .err()
            .map(|err| format!("{err:#}")),
        Err(err) => Some(format!("{err:#}")),
    };
    engine.finish();
    engine.snapshot.error = error;
    if let Ok(Some(store)) = &mut store
        && let Err(err) = store.save(&mut engine)
    {
        let error = format!("Ошибка сохранения истории: {err:#}");
        if let Some(previous) = &mut engine.snapshot.error {
            *previous = format!("{previous}\n{error}");
        } else {
            engine.snapshot.error = Some(error);
        }
    }
    engine.snapshot
}

fn publish(
    engine: &mut Engine,
    store: Option<&mut Store>,
    shared: &RwLock<Arc<Snapshot>>,
) -> anyhow::Result<()> {
    if let Some(store) = store {
        store.save(engine).context("Ошибка сохранения истории")?;
    }
    *shared.write().unwrap() = Arc::new(engine.snapshot.clone());
    Ok(())
}

fn run(
    e: &mut Engine,
    mut store: Option<&mut Store>,
    stop: &AtomicBool,
    shared: &RwLock<Arc<Snapshot>>,
) -> anyhow::Result<()> {
    let (tx, rx) = crossbeam_channel::bounded(131_072);
    let mut collector = Collector::start(tx)?;
    // Refresh after enabling providers to close the process-start/snapshot race.
    for p in collector::processes() {
        e.register(p);
    }
    let mut pending = Vec::new();
    let mut last_publish = Instant::now();
    let mut result = publish(e, store.as_deref_mut(), shared);
    while result.is_ok() && !stop.load(Ordering::Relaxed) {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
            pending.push(event);
        }
        pending.extend(rx.try_iter().take(16_384));
        if pending.len() > 262_144 {
            result = Err(anyhow::anyhow!(
                "Поток событий превысил лимит буфера. Мониторинг остановлен."
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
                e.snapshot
                    .recording
                    .as_mut()
                    .unwrap()
                    .health
                    .messages
                    .push("Достигнут лимит 50 000 строк. Запустите мониторинг заново.".into());
                break;
            }
            result = publish(e, store.as_deref_mut(), shared);
            last_publish = Instant::now();
        }
    }
    collector.stop(&mut e.snapshot.recording.as_mut().unwrap().health);
    pending.extend(rx.try_iter());
    pending.sort_by_key(|event| event.ticks);
    for event in pending {
        e.apply(event.event);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe(history_dir: Option<&Path>) -> Snapshot {
        let at = now_ms();
        let engine = Engine::new(
            CaptureConfig {
                exe_filters: vec!["test.exe".into()],
                include_children: false,
                udp_idle_seconds: 30,
                label: "Тест".into(),
            },
            vec![ProcessInfo {
                pid: 42,
                started: at - 1000,
                exe: "test.exe".into(),
                ..ProcessInfo::default()
            }],
        );
        let shared = RwLock::new(Arc::new(Snapshot::default()));
        capture(engine, history_dir, |engine, store| {
            assert_eq!(store.is_some(), history_dir.is_some());
            let udp = |kind, bytes| Event::Network {
                at,
                pid: 42,
                protocol: Protocol::Udp,
                local: "127.0.0.1:50000".parse().unwrap(),
                remote: "127.0.0.1:30000".parse().unwrap(),
                bytes,
                kind,
            };
            engine.apply(udp(NetKind::Send, 32));
            engine.apply(Event::Dns {
                at,
                pid: 42,
                name: "example.test".into(),
                query_type: 1,
                status: 0,
                results: "127.0.0.1".into(),
                activity: 0,
                completed: true,
            });
            publish(engine, store, &shared)?;
            let running = shared.read().unwrap().clone();
            assert!(running.running);
            assert_eq!(running.flows[0].sent, 32);
            assert_eq!(running.dns[0].name, "example.test");
            engine.apply(udp(NetKind::Receive, 12));
            Ok(())
        })
    }

    #[test]
    fn monitor_publishes_and_finishes_with_history_opt_in() {
        let dir = std::env::temp_dir().join(format!(
            "netmon-runtime-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let live = observe(None);
        assert!(!live.running);
        assert!(live.recording.as_ref().unwrap().ended.is_some());
        assert_eq!(live.flows[0].received, 12);
        assert_eq!(live.dns.len(), 1);
        let export = crate::storage::export(&live, &dir).unwrap();
        assert!(export.join("recording.json").is_file());
        assert!(Store::list_existing(&dir).unwrap().is_empty());
        assert!(!dir.join("netmon.sqlite3").exists());

        let recorded = observe(Some(&dir));
        assert!(recorded.error.is_none());
        let store = Store::open(&dir).unwrap();
        let history = store.list().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, recorded.recording.unwrap().id);
        let saved = store.load(&history[0].id).unwrap();
        assert!(saved.recording.unwrap().ended.is_some());
        assert_eq!(saved.flows[0].received, live.flows[0].received);
        assert_eq!(saved.dns[0].name, live.dns[0].name);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
