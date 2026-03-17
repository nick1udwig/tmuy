use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Live,
    Exited,
    Failed,
}

impl SessionStatus {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Starting | Self::Live)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NetworkMode {
    On,
    Off,
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::On
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FsGrant {
    Full,
    ReadOnly(PathBuf),
    ReadWrite(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxSpec {
    pub fs: Vec<FsGrant>,
    pub net: NetworkMode,
}

impl Default for SandboxSpec {
    fn default() -> Self {
        Self {
            fs: vec![FsGrant::Full],
            net: NetworkMode::On,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CommandMode {
    Shell,
    OneShot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id_hash: String,
    pub started_name: String,
    pub current_name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub mode: CommandMode,
    pub sandbox: SandboxSpec,
    pub status: SessionStatus,
    pub started_log_dir: PathBuf,
    pub meta_path: PathBuf,
    pub log_path: PathBuf,
    pub events_path: PathBuf,
    pub socket_path: PathBuf,
    pub service_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub env: BTreeMap<String, String>,
    pub detach_key: String,
}

impl SessionRecord {
    pub fn short_ref(&self) -> String {
        format!("{} ({})", self.current_name, self.id_hash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StateFile {
    pub next_numeric_name: u64,
    pub sessions: Vec<SessionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    LiveOnly,
    DeadOnly,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub detail: serde_json::Value,
}
