use crate::{collector, engine::Engine, model::*};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const DATABASE_FILE: &str = "netmon.sqlite3";

pub fn data_dir() -> PathBuf {
    std::env::var_os("NETMON_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("netmon.exe"))
                .parent()
                .unwrap_or(Path::new("."))
                .join("data")
        })
}
pub struct Store {
    db: Connection,
}
impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).context("Создание папки истории")?;
        let db = Connection::open(dir.join(DATABASE_FILE)).context("Открытие истории SQLite")?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS recordings (id TEXT PRIMARY KEY, started INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS flows (recording_id TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE, id INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(recording_id,id));
            CREATE TABLE IF NOT EXISTS dns (recording_id TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE, id INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(recording_id,id));
            PRAGMA user_version=1;")?;
        Ok(Self { db })
    }

    pub fn save(&mut self, e: &mut Engine) -> Result<()> {
        let r = e.snapshot.recording.as_ref().context("Нет записи")?;
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO recordings VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![r.id, r.started, serde_json::to_string(r)?])?;
        {
            let mut stmt = tx.prepare_cached("INSERT INTO flows VALUES (?1,?2,?3) ON CONFLICT(recording_id,id) DO UPDATE SET payload=excluded.payload")?;
            for &i in &e.dirty {
                let f = &e.snapshot.flows[i];
                stmt.execute(params![r.id, f.id, serde_json::to_string(f)?])?;
            }
            let mut stmt = tx.prepare_cached("INSERT INTO dns VALUES (?1,?2,?3)")?;
            for d in &e.snapshot.dns[e.saved_dns..] {
                stmt.execute(params![r.id, d.id, serde_json::to_string(d)?])?;
            }
        }
        tx.commit()?;
        e.dirty.clear();
        e.saved_dns = e.snapshot.dns.len();
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Recording>> {
        let mut stmt = self
            .db
            .prepare("SELECT payload FROM recordings ORDER BY started DESC LIMIT 500")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn list_existing(dir: &Path) -> Result<Vec<Recording>> {
        if dir.join(DATABASE_FILE).try_exists()? {
            Self::open(dir)?.list()
        } else {
            Ok(Vec::new())
        }
    }

    pub fn load(&self, id: &str) -> Result<Snapshot> {
        let text: String =
            self.db
                .query_row("SELECT payload FROM recordings WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
        let mut r: Recording = serde_json::from_str(&text)?;
        if r.ended.is_none() {
            r.health
                .messages
                .push("Запись не была завершена штатно: конец наблюдения неизвестен.".into());
        }
        let mut s = Snapshot {
            recording: Some(r),
            ..Default::default()
        };
        let mut stmt = self
            .db
            .prepare("SELECT payload FROM flows WHERE recording_id=?1 ORDER BY id")?;
        for row in stmt.query_map([id], |r| r.get::<_, String>(0))? {
            s.flows.push(serde_json::from_str(&row?)?);
        }
        let mut stmt = self
            .db
            .prepare("SELECT payload FROM dns WHERE recording_id=?1 ORDER BY id")?;
        for row in stmt.query_map([id], |r| r.get::<_, String>(0))? {
            s.dns.push(serde_json::from_str(&row?)?);
        }
        Ok(s)
    }

    pub fn delete(&mut self, id: &str, active_recording_id: Option<&str>) -> Result<bool> {
        let tx = self.db.transaction()?;
        let payload = tx.query_row("SELECT payload FROM recordings WHERE id=?1", [id], |row| {
            row.get::<_, String>(0)
        });
        let recording: Recording = match payload {
            Ok(payload) => serde_json::from_str(&payload)?,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        if active_recording_id == Some(id)
            || (recording.ended.is_none()
                && recording_owner_running(&recording, &collector::processes()))
        {
            return Ok(false);
        }
        let deleted = tx.execute("DELETE FROM recordings WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(deleted != 0)
    }
}

pub fn active_recording_ids(
    recordings: &[Recording],
    current_recording_id: Option<&str>,
) -> HashSet<String> {
    let mut active: HashSet<_> = recordings
        .iter()
        .filter(|recording| current_recording_id == Some(recording.id.as_str()))
        .map(|recording| recording.id.clone())
        .collect();
    let unfinished: Vec<_> = recordings
        .iter()
        .filter(|recording| recording.ended.is_none() && !active.contains(&recording.id))
        .collect();
    if unfinished.is_empty() {
        return active;
    }
    let processes = collector::processes();
    active.extend(
        unfinished
            .into_iter()
            .filter(|recording| recording_owner_running(recording, &processes))
            .map(|recording| recording.id.clone()),
    );
    active
}

fn recording_owner_running(recording: &Recording, processes: &[ProcessInfo]) -> bool {
    if recording.ended.is_some() {
        return false;
    }
    let Some(pid) = recording
        .id
        .rsplit_once('-')
        .and_then(|(_, pid)| pid.parse::<u32>().ok())
    else {
        return false;
    };
    processes.iter().any(|process| {
        process.pid == pid && (process.started == 0 || process.started <= recording.started)
    })
}

pub fn export(snapshot: &Snapshot, dir: &Path) -> Result<PathBuf> {
    let r = snapshot
        .recording
        .as_ref()
        .context("Сначала выберите запись")?;
    let dir = dir.join("exports").join(format!("{}-{}", r.id, now_ms()));
    fs::create_dir_all(&dir)?;
    serde_json::to_writer_pretty(fs::File::create(dir.join("recording.json"))?, snapshot)?;
    let mut csv = csv::Writer::from_path(dir.join("connections.csv"))?;
    csv.write_record([
        "exe",
        "pid",
        "path",
        "process_started_utc_ms",
        "protocol",
        "local",
        "remote",
        "first_utc_ms",
        "last_utc_ms",
        "outcome",
        "end_reason",
        "ntstatus",
        "sent_bytes",
        "received_bytes",
        "retransmit_bytes",
        "partial",
        "source",
    ])?;
    for f in &snapshot.flows {
        csv.write_record([
            f.process.exe.clone(),
            f.process.pid.to_string(),
            f.process.path.clone(),
            f.process.started.to_string(),
            f.protocol.to_string(),
            f.local.map(|x| x.to_string()).unwrap_or_default(),
            f.remote.to_string(),
            f.first.to_string(),
            f.last.to_string(),
            f.status(),
            f.end_reason.clone(),
            f.error.map(|s| format!("0x{s:08X}")).unwrap_or_default(),
            f.sent.to_string(),
            f.received.to_string(),
            f.retransmit_bytes.to_string(),
            f.partial.to_string(),
            f.source.clone(),
        ])?;
    }
    csv.flush()?;
    let mut csv = csv::Writer::from_path(dir.join("dns.csv"))?;
    csv.write_record([
        "exe",
        "pid",
        "time_utc_ms",
        "name",
        "type",
        "status",
        "results",
        "duration_ms",
        "source",
        "request_time_utc_ms",
    ])?;
    for d in &snapshot.dns {
        csv.write_record([
            d.process.exe.clone(),
            d.process.pid.to_string(),
            d.at.to_string(),
            d.name.clone(),
            dns_type(d.query_type),
            d.status
                .map(|status| status.to_string())
                .unwrap_or_default(),
            d.results.clone(),
            d.duration_ms.map(|d| d.to_string()).unwrap_or_default(),
            d.source.clone(),
            d.request_at.map(|at| at.to_string()).unwrap_or_default(),
        ])?;
    }
    csv.flush()?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(id: &str, started: i64, ended: Option<i64>) -> Recording {
        Recording {
            id: id.into(),
            started,
            ended,
            config: CaptureConfig {
                exe_filters: vec!["test.exe".into()],
                include_children: false,
                udp_idle_seconds: 30,
                label: id.into(),
            },
            health: Health::default(),
        }
    }

    #[test]
    fn incremental_save_and_reload() {
        let dir =
            std::env::temp_dir().join(format!("netmon-test-{}-{}", std::process::id(), now_ms()));
        let mut store = Store::open(&dir).unwrap();
        let mut e = Engine::new(
            CaptureConfig {
                exe_filters: vec!["*".into()],
                include_children: false,
                udp_idle_seconds: 30,
                label: "Тест".into(),
            },
            vec![],
        );
        e.apply(Event::Dns {
            at: 100,
            pid: 9999999,
            name: "пример.test".into(),
            query_type: 1,
            status: 9003,
            results: String::new(),
            activity: 0,
            completed: true,
        });
        store.save(&mut e).unwrap();
        store.save(&mut e).unwrap();
        e.finish();
        store.save(&mut e).unwrap();
        let r = store.list().unwrap();
        assert_eq!(r.len(), 1);
        let loaded = store.load(&r[0].id).unwrap();
        assert_eq!(loaded.dns.len(), 1);
        assert_eq!(loaded.dns[0].name, "пример.test");
        assert!(loaded.recording.unwrap().ended.is_some());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_cascades_without_touching_neighbors_or_active_recording() {
        let dir = std::env::temp_dir().join(format!(
            "netmon-delete-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut store = Store::open(&dir).unwrap();
        for r in [
            recording("older", 10, Some(11)),
            recording("interrupted", 20, None),
            recording("active", 30, None),
        ] {
            store
                .db
                .execute(
                    "INSERT INTO recordings VALUES (?1,?2,?3)",
                    params![r.id, r.started, serde_json::to_string(&r).unwrap()],
                )
                .unwrap();
            store
                .db
                .execute("INSERT INTO flows VALUES (?1,1,'{}')", [&r.id])
                .unwrap();
            store
                .db
                .execute("INSERT INTO dns VALUES (?1,1,'{}')", [&r.id])
                .unwrap();
        }

        assert!(store.delete("interrupted", Some("active")).unwrap());
        assert!(!store.delete("active", Some("active")).unwrap());
        assert!(!store.delete("missing", Some("active")).unwrap());

        let ids: Vec<_> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["active", "older"]);
        for table in ["flows", "dns"] {
            let count: i64 = store
                .db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 2, "unexpected rows in {table}");
        }
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unfinished_recording_owner_distinguishes_running_process_from_pid_reuse() {
        let unfinished = recording("20261122-120000-000-42", 2_000, None);
        let owner = ProcessInfo {
            pid: 42,
            started: 1_000,
            ..Default::default()
        };
        assert!(recording_owner_running(&unfinished, &[owner]));

        let reused_pid = ProcessInfo {
            pid: 42,
            started: 3_000,
            ..Default::default()
        };
        assert!(!recording_owner_running(&unfinished, &[reused_pid]));
        assert!(!recording_owner_running(&unfinished, &[]));
        assert!(recording_owner_running(
            &unfinished,
            &[ProcessInfo {
                pid: 42,
                started: 0,
                ..Default::default()
            }]
        ));
        assert!(!recording_owner_running(
            &recording("20261122-120000-000-42", 2_000, Some(2_500)),
            &[ProcessInfo {
                pid: 42,
                started: 1_000,
                ..Default::default()
            }]
        ));
    }
}
