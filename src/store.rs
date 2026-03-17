use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, Utc};
use fs2::FileExt;
use rand::Rng;

use crate::model::{
    CommandMode, EventRecord, FsGrant, NetworkMode, SandboxSpec, SessionRecord, SessionScope,
    SessionStatus, StateFile,
};

#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    pub explicit_name: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub mode: CommandMode,
    pub sandbox: SandboxSpec,
    pub detach_key: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    base_dir: PathBuf,
}

impl Store {
    pub fn new() -> Result<Self> {
        let base_dir = if let Some(override_dir) = std::env::var_os("TMUY_HOME") {
            PathBuf::from(override_dir)
        } else {
            dirs::home_dir()
                .ok_or_else(|| anyhow!("could not determine home directory"))?
                .join(".tmuy")
        };
        Self::from_base_dir(base_dir)
    }

    pub fn from_base_dir(base_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(base_dir.join("live"))?;
        Ok(Self { base_dir })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn live_dir(&self) -> PathBuf {
        self.base_dir.join("live")
    }

    pub fn state_path(&self) -> PathBuf {
        self.base_dir.join("state.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.base_dir.join("state.lock")
    }

    fn with_locked_state<T>(&self, mut f: impl FnMut(&mut StateFile) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.base_dir)?;
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.lock_path())?;
        lock_file.lock_exclusive()?;

        let state_path = self.state_path();
        let mut state = if state_path.exists() {
            let raw = fs::read(&state_path)?;
            if raw.is_empty() {
                StateFile::default()
            } else {
                serde_json::from_slice::<StateFile>(&raw)
                    .with_context(|| format!("failed to parse {}", state_path.display()))?
            }
        } else {
            StateFile::default()
        };

        let result = f(&mut state)?;

        let tmp_path = state_path.with_extension("json.tmp");
        fs::write(&tmp_path, serde_json::to_vec_pretty(&state)?)?;
        fs::rename(tmp_path, &state_path)?;
        lock_file.unlock()?;
        Ok(result)
    }

    pub fn create_session(&self, req: CreateSessionRequest) -> Result<SessionRecord> {
        self.with_locked_state(|state| {
            let name =
                match &req.explicit_name {
                    Some(name) => {
                        validate_name(name)?;
                        if state.sessions.iter().any(|session| {
                            session.current_name == *name && session.status.is_live()
                        }) {
                            bail!("live session name already exists: {name}");
                        }
                        name.clone()
                    }
                    None => {
                        let next = state.next_numeric_name.max(1);
                        state.next_numeric_name = next + 1;
                        next.to_string()
                    }
                };

            let id_hash = loop {
                let candidate = format!("{:07x}", rand::rng().random_range(0..=0x0fff_ffff));
                if state
                    .sessions
                    .iter()
                    .all(|session| session.id_hash != candidate)
                {
                    break candidate;
                }
            };

            let now = Utc::now();
            let dated_root = self
                .base_dir
                .join(format!("{:04}", now.year()))
                .join(format!("{:02}", now.month()))
                .join(format!("{:02}", now.day()));
            let started_name = name.clone();
            let log_dir =
                dated_root.join(format!("{}-{}", sanitize_for_path(&started_name), id_hash));
            fs::create_dir_all(&log_dir)?;

            let log_path = log_dir.join("pty.log");
            let meta_path = log_dir.join("meta.json");
            let events_path = log_dir.join("events.jsonl");
            let socket_path = self.live_dir().join(format!("{id_hash}.sock"));

            let mut env = req.env.clone();
            env.insert("TMUY_SESSION_HASH".to_string(), id_hash.clone());
            env.insert(
                "TMUY_SESSION_STARTED_NAME".to_string(),
                started_name.clone(),
            );

            let record = SessionRecord {
                id_hash,
                started_name,
                current_name: name,
                created_at: now,
                updated_at: now,
                cwd: req.cwd.clone(),
                command: req.command.clone(),
                mode: req.mode.clone(),
                sandbox: req.sandbox.clone(),
                status: SessionStatus::Starting,
                started_log_dir: log_dir,
                meta_path,
                log_path,
                events_path,
                socket_path,
                service_pid: None,
                child_pid: None,
                exit_code: None,
                failure_reason: None,
                env,
                detach_key: req.detach_key.clone(),
            };

            state.sessions.push(record.clone());
            self.write_meta(&record)?;
            self.append_event(
                &record,
                EventRecord {
                    ts: now,
                    kind: "created".to_string(),
                    detail: serde_json::json!({
                        "name": record.current_name,
                        "mode": record.mode,
                        "sandbox": record.sandbox,
                    }),
                },
            )?;

            Ok(record)
        })
    }

    pub fn session_by_hash(&self, hash: &str) -> Result<SessionRecord> {
        self.with_locked_state(|state| {
            state
                .sessions
                .iter()
                .find(|session| session.id_hash == hash)
                .cloned()
                .ok_or_else(|| anyhow!("unknown session hash: {hash}"))
        })
    }

    pub fn resolve_target(&self, target: &str, scope: SessionScope) -> Result<SessionRecord> {
        self.with_locked_state(|state| resolve_target_in_state(state, target, scope))
    }

    pub fn list_sessions(&self, scope: SessionScope) -> Result<Vec<SessionRecord>> {
        self.with_locked_state(|state| {
            let mut sessions: Vec<SessionRecord> = state
                .sessions
                .iter()
                .filter(|session| match scope {
                    SessionScope::LiveOnly => session.status.is_live(),
                    SessionScope::DeadOnly => !session.status.is_live(),
                    SessionScope::All => true,
                })
                .cloned()
                .collect();
            sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Ok(sessions)
        })
    }

    pub fn mark_live(
        &self,
        hash: &str,
        service_pid: u32,
        child_pid: Option<u32>,
    ) -> Result<SessionRecord> {
        self.update_session(hash, |session| {
            session.status = SessionStatus::Live;
            session.service_pid = Some(service_pid);
            session.child_pid = child_pid;
            session.updated_at = Utc::now();
            session.failure_reason = None;
        })
    }

    pub fn mark_exited(&self, hash: &str, exit_code: Option<i32>) -> Result<SessionRecord> {
        self.update_session(hash, |session| {
            session.status = SessionStatus::Exited;
            session.exit_code = exit_code;
            session.updated_at = Utc::now();
        })
    }

    pub fn mark_failed(&self, hash: &str, reason: impl Into<String>) -> Result<SessionRecord> {
        let reason = reason.into();
        let failed = self.update_session(hash, |session| {
            session.status = SessionStatus::Failed;
            session.failure_reason = Some(reason.clone());
            session.updated_at = Utc::now();
        })?;
        self.append_event(
            &failed,
            EventRecord {
                ts: Utc::now(),
                kind: "failed".to_string(),
                detail: serde_json::json!({
                    "reason": failed.failure_reason,
                }),
            },
        )?;
        Ok(failed)
    }

    pub fn rename_session(&self, current_target: &str, new_name: &str) -> Result<SessionRecord> {
        validate_name(new_name)?;
        let renamed = self.with_locked_state(|state| {
            if state
                .sessions
                .iter()
                .any(|session| session.current_name == new_name && session.status.is_live())
            {
                bail!("live session name already exists: {new_name}");
            }

            let target = resolve_target_in_state(state, current_target, SessionScope::LiveOnly)?;
            let session = state
                .sessions
                .iter_mut()
                .find(|session| session.id_hash == target.id_hash)
                .ok_or_else(|| anyhow!("unknown session target: {current_target}"))?;
            session.current_name = new_name.to_string();
            session.updated_at = Utc::now();
            Ok(session.clone())
        })?;

        self.write_meta(&renamed)?;
        self.append_event(
            &renamed,
            EventRecord {
                ts: Utc::now(),
                kind: "renamed".to_string(),
                detail: serde_json::json!({
                    "current_name": renamed.current_name,
                    "started_name": renamed.started_name,
                    "id_hash": renamed.id_hash,
                }),
            },
        )?;
        Ok(renamed)
    }

    pub fn update_session(
        &self,
        hash: &str,
        mut mutate: impl FnMut(&mut SessionRecord),
    ) -> Result<SessionRecord> {
        let updated = self.with_locked_state(|state| {
            let session = state
                .sessions
                .iter_mut()
                .find(|session| session.id_hash == hash)
                .ok_or_else(|| anyhow!("unknown session hash: {hash}"))?;
            mutate(session);
            Ok(session.clone())
        })?;
        self.write_meta(&updated)?;
        Ok(updated)
    }

    pub fn append_event(&self, session: &SessionRecord, event: EventRecord) -> Result<()> {
        if let Some(parent) = session.events_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&session.events_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &event)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    pub fn write_meta(&self, session: &SessionRecord) -> Result<()> {
        if let Some(parent) = session.meta_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(&session.meta_path)?;
        serde_json::to_writer_pretty(file, session)?;
        Ok(())
    }
}

fn resolve_target_in_state(
    state: &StateFile,
    target: &str,
    scope: SessionScope,
) -> Result<SessionRecord> {
    let hash_match = state
        .sessions
        .iter()
        .find(|session| session.id_hash == target && session_matches_scope(session, scope))
        .cloned();
    let name_match = resolve_name_in_state(state, target, scope);

    match (hash_match, name_match) {
        (Some(hash_session), Ok(name_session)) if hash_session.id_hash != name_session.id_hash => {
            bail!(
                "ambiguous session target: {target}; it matches hash {} and name {}",
                hash_session.short_ref(),
                name_session.short_ref()
            )
        }
        (Some(hash_session), _) => Ok(hash_session),
        (None, Ok(name_session)) => Ok(name_session),
        (None, Err(name_err)) => Err(name_err),
    }
}

fn resolve_name_in_state(
    state: &StateFile,
    name: &str,
    scope: SessionScope,
) -> Result<SessionRecord> {
    let matches: Vec<SessionRecord> = state
        .sessions
        .iter()
        .filter(|session| session.current_name == name)
        .filter(|session| session_matches_scope(session, scope))
        .cloned()
        .collect();

    match matches.as_slice() {
        [] => Err(anyhow!("unknown session target: {name}")),
        [single] => Ok(single.clone()),
        _ => {
            if scope == SessionScope::All {
                if let Some(live) = matches.iter().find(|session| session.status.is_live()) {
                    return Ok(live.clone());
                }
            }
            Err(anyhow!(
                "ambiguous session target: {name}; use a unique current name or the stable session hash"
            ))
        }
    }
}

fn session_matches_scope(session: &SessionRecord, scope: SessionScope) -> bool {
    match scope {
        SessionScope::LiveOnly => session.status.is_live(),
        SessionScope::DeadOnly => !session.status.is_live(),
        SessionScope::All => true,
    }
}

pub fn parse_sandbox(
    fs_flags: &[String],
    net_flag: Option<&str>,
    cwd: &Path,
) -> Result<SandboxSpec> {
    let net = match net_flag {
        None | Some("on") => NetworkMode::On,
        Some("off") => NetworkMode::Off,
        Some(other) => bail!("invalid --net value: {other}; use on|off"),
    };

    if fs_flags.is_empty() {
        return Ok(SandboxSpec {
            fs: vec![FsGrant::Full],
            net,
        });
    }

    let mut grants = Vec::new();
    for raw in fs_flags {
        if matches!(grants.as_slice(), [FsGrant::Full]) {
            bail!("--fs full cannot be mixed with ro:/rw: grants");
        }
        if raw == "full" {
            if !grants.is_empty() {
                bail!("--fs full cannot be mixed with ro:/rw: grants");
            }
            grants.push(FsGrant::Full);
            continue;
        }

        let (mode, raw_path) = raw
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid --fs value: {raw}; use full|ro:<path>|rw:<path>"))?;
        let path = absolutize_from_cwd(raw_path, cwd);
        match mode {
            "ro" => grants.push(FsGrant::ReadOnly(path)),
            "rw" => grants.push(FsGrant::ReadWrite(path)),
            _ => bail!("invalid --fs mode: {mode}; use full|ro:<path>|rw:<path>"),
        }
    }

    Ok(SandboxSpec { fs: grants, net })
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name cannot be empty");
    }
    let valid = name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if !valid {
        bail!("session names may only contain ASCII letters, numbers, '.', '-' and '_'");
    }
    Ok(())
}

pub fn sanitize_for_path(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn absolutize_from_cwd(raw_path: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::*;
    use crate::model::{CommandMode, SessionScope, SessionStatus};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn make_store() -> (tempfile::TempDir, Store) {
        let tmp = tempdir().unwrap();
        let store = Store::from_base_dir(tmp.path().join(".tmuy")).unwrap();
        (tmp, store)
    }

    fn base_request(name: Option<&str>) -> CreateSessionRequest {
        CreateSessionRequest {
            explicit_name: name.map(ToOwned::to_owned),
            cwd: PathBuf::from("/tmp/project"),
            command: vec!["/bin/bash".to_string(), "-i".to_string()],
            mode: CommandMode::Shell,
            sandbox: SandboxSpec::default(),
            detach_key: "C-b d".to_string(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn auto_names_increment() {
        let (_tmp, store) = make_store();
        let first = store.create_session(base_request(None)).unwrap();
        let second = store.create_session(base_request(None)).unwrap();
        assert_eq!(first.current_name, "1");
        assert_eq!(second.current_name, "2");
    }

    #[test]
    fn create_session_writes_created_event() {
        let (_tmp, store) = make_store();
        let created = store.create_session(base_request(Some("alpha"))).unwrap();
        let events = std::fs::read_to_string(created.events_path).unwrap();
        assert!(events.contains("\"kind\":\"created\""));
        assert!(events.contains("\"name\":\"alpha\""));
    }

    #[test]
    fn rename_keeps_hash_and_started_path() {
        let (_tmp, store) = make_store();
        let created = store.create_session(base_request(Some("alpha"))).unwrap();
        let renamed = store.rename_session("alpha", "beta").unwrap();
        assert_eq!(created.id_hash, renamed.id_hash);
        assert_eq!(created.started_name, renamed.started_name);
        assert_eq!(created.started_log_dir, renamed.started_log_dir);
        assert_eq!(renamed.current_name, "beta");
        let events = std::fs::read_to_string(renamed.events_path).unwrap();
        assert!(events.contains("\"kind\":\"renamed\""));
        assert!(events.contains("\"current_name\":\"beta\""));
    }

    #[test]
    fn live_collision_is_rejected_but_dead_name_can_reuse() {
        let (_tmp, store) = make_store();
        let first = store.create_session(base_request(Some("alpha"))).unwrap();
        let err = store
            .create_session(base_request(Some("alpha")))
            .unwrap_err();
        assert!(err.to_string().contains("live session name already exists"));
        store
            .update_session(&first.id_hash, |session| {
                session.status = SessionStatus::Exited
            })
            .unwrap();
        let second = store.create_session(base_request(Some("alpha"))).unwrap();
        assert_ne!(first.id_hash, second.id_hash);
    }

    #[test]
    fn list_filters_work() {
        let (_tmp, store) = make_store();
        let live = store.create_session(base_request(Some("live"))).unwrap();
        let dead = store.create_session(base_request(Some("dead"))).unwrap();
        store.mark_exited(&dead.id_hash, Some(0)).unwrap();

        let live_only = store.list_sessions(SessionScope::LiveOnly).unwrap();
        let dead_only = store.list_sessions(SessionScope::DeadOnly).unwrap();
        let all = store.list_sessions(SessionScope::All).unwrap();

        assert_eq!(live_only.len(), 1);
        assert_eq!(live_only[0].id_hash, live.id_hash);
        assert_eq!(dead_only.len(), 1);
        assert_eq!(dead_only[0].id_hash, dead.id_hash);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn parse_sandbox_flags() {
        let parsed = parse_sandbox(
            &["ro:src".to_string(), "rw:target".to_string()],
            Some("off"),
            Path::new("/work"),
        )
        .unwrap();
        assert_eq!(parsed.net, NetworkMode::Off);
        assert_eq!(
            parsed.fs,
            vec![
                FsGrant::ReadOnly(PathBuf::from("/work/src")),
                FsGrant::ReadWrite(PathBuf::from("/work/target"))
            ]
        );
    }

    #[test]
    fn store_new_uses_env_override_and_home_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("TMUY_HOME", tmp.path().join("override"));
        }
        let env_store = Store::new().unwrap();
        assert!(env_store.base_dir().ends_with("override"));

        unsafe {
            std::env::remove_var("TMUY_HOME");
        }
        let home_store = Store::new().unwrap();
        assert!(home_store.base_dir().ends_with(".tmuy"));
    }

    #[test]
    fn empty_state_file_is_treated_as_default() {
        let (_tmp, store) = make_store();
        std::fs::write(store.state_path(), b"").unwrap();
        let sessions = store.list_sessions(SessionScope::All).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn invalid_state_file_returns_parse_error() {
        let (_tmp, store) = make_store();
        std::fs::write(store.state_path(), b"{invalid").unwrap();
        let err = store.list_sessions(SessionScope::All).unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn session_by_hash_unknown_errors() {
        let (_tmp, store) = make_store();
        let err = store.session_by_hash("missing").unwrap_err();
        assert!(err.to_string().contains("unknown session hash"));
    }

    #[test]
    fn resolve_target_accepts_name_and_hash() {
        let (_tmp, store) = make_store();
        let created = store.create_session(base_request(Some("same"))).unwrap();

        let by_name = store
            .resolve_target("same", SessionScope::LiveOnly)
            .unwrap();
        let by_hash = store
            .resolve_target(&created.id_hash, SessionScope::LiveOnly)
            .unwrap();

        assert_eq!(by_name.id_hash, created.id_hash);
        assert_eq!(by_hash.id_hash, created.id_hash);
    }

    #[test]
    fn resolve_target_prefers_live_name_in_all_scope() {
        let (_tmp, store) = make_store();
        let dead = store.create_session(base_request(Some("same"))).unwrap();
        store.mark_exited(&dead.id_hash, Some(0)).unwrap();
        let live = store.create_session(base_request(Some("same"))).unwrap();
        let resolved = store.resolve_target("same", SessionScope::All).unwrap();
        assert_eq!(resolved.id_hash, live.id_hash);
    }

    #[test]
    fn resolve_target_errors_when_duplicate_dead_names_exist() {
        let (_tmp, store) = make_store();
        let first = store.create_session(base_request(Some("same"))).unwrap();
        store.mark_exited(&first.id_hash, Some(0)).unwrap();
        let second = store.create_session(base_request(Some("same"))).unwrap();
        store.mark_exited(&second.id_hash, Some(0)).unwrap();
        let err = store.resolve_target("same", SessionScope::All).unwrap_err();
        assert!(err.to_string().contains("ambiguous session target"));
    }

    #[test]
    fn resolve_target_dead_only_returns_dead_session() {
        let (_tmp, store) = make_store();
        let created = store.create_session(base_request(Some("dead"))).unwrap();
        store.mark_exited(&created.id_hash, Some(0)).unwrap();
        let resolved = store
            .resolve_target("dead", SessionScope::DeadOnly)
            .unwrap();
        assert_eq!(resolved.id_hash, created.id_hash);
        assert_eq!(resolved.status, SessionStatus::Exited);
    }

    #[test]
    fn mark_failed_and_mark_live_update_failure_reason() {
        let (_tmp, store) = make_store();
        let created = store.create_session(base_request(Some("alpha"))).unwrap();
        let failed = store.mark_failed(&created.id_hash, "boom").unwrap();
        assert_eq!(failed.status, SessionStatus::Failed);
        assert_eq!(failed.failure_reason.as_deref(), Some("boom"));
        let events = std::fs::read_to_string(&failed.events_path).unwrap();
        assert!(events.contains("\"kind\":\"failed\""));
        assert!(events.contains("\"reason\":\"boom\""));
        let live = store.mark_live(&created.id_hash, 10, Some(11)).unwrap();
        assert_eq!(live.status, SessionStatus::Live);
        assert_eq!(live.failure_reason, None);
    }

    #[test]
    fn rename_unknown_and_collision_error() {
        let (_tmp, store) = make_store();
        let err = store.rename_session("missing", "next").unwrap_err();
        assert!(err.to_string().contains("unknown session target"));

        store.create_session(base_request(Some("one"))).unwrap();
        store.create_session(base_request(Some("two"))).unwrap();
        let err = store.rename_session("one", "two").unwrap_err();
        assert!(err.to_string().contains("live session name already exists"));
    }

    #[test]
    fn rename_accepts_hash_target() {
        let (_tmp, store) = make_store();
        let created = store.create_session(base_request(Some("alpha"))).unwrap();
        let renamed = store.rename_session(&created.id_hash, "beta").unwrap();
        assert_eq!(renamed.current_name, "beta");
        assert_eq!(renamed.id_hash, created.id_hash);
    }

    #[test]
    fn update_unknown_hash_errors() {
        let (_tmp, store) = make_store();
        let err = store
            .update_session("missing", |session| session.current_name = "x".to_string())
            .unwrap_err();
        assert!(err.to_string().contains("unknown session hash"));
    }

    #[test]
    fn append_event_and_write_meta_create_parent_dirs() {
        let (_tmp, store) = make_store();
        let mut session = store.create_session(base_request(Some("alpha"))).unwrap();
        session.events_path = store.base_dir().join("nested/events/out.jsonl");
        session.meta_path = store.base_dir().join("nested/meta/out.json");
        store
            .append_event(
                &session,
                EventRecord {
                    ts: Utc::now(),
                    kind: "x".to_string(),
                    detail: serde_json::json!({"ok": true}),
                },
            )
            .unwrap();
        store.write_meta(&session).unwrap();
        assert!(session.events_path.exists());
        assert!(session.meta_path.exists());
    }

    #[test]
    fn parse_sandbox_validation_errors() {
        let err = parse_sandbox(&[], Some("bad"), Path::new("/work")).unwrap_err();
        assert!(err.to_string().contains("invalid --net"));

        let err = parse_sandbox(
            &["full".to_string(), "ro:src".to_string()],
            None,
            Path::new("/work"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot be mixed"));

        let err = parse_sandbox(&["oops".to_string()], None, Path::new("/work")).unwrap_err();
        assert!(err.to_string().contains("invalid --fs value"));

        let err = parse_sandbox(&["xx:src".to_string()], None, Path::new("/work")).unwrap_err();
        assert!(err.to_string().contains("invalid --fs mode"));

        let err = parse_sandbox(
            &["ro:src".to_string(), "full".to_string()],
            None,
            Path::new("/work"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot be mixed"));
    }

    #[test]
    fn validate_name_and_path_helpers_cover_error_cases() {
        let err = validate_name("").unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));
        let err = validate_name("bad/name").unwrap_err();
        assert!(err.to_string().contains("may only contain"));

        assert_eq!(sanitize_for_path("bad/name"), "bad_name");
        assert_eq!(
            absolutize_from_cwd("/abs", Path::new("/work")),
            PathBuf::from("/abs")
        );
    }
}
