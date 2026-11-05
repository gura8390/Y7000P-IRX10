//! Owned application state and bounded requests. No UI callback owns native I/O.
use crate::service_engine::Engine;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub available: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    pub value: Value,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub unit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub title: String,
    pub value: ArcValue,
    pub writable: bool,
    pub reason: String,
}
/// Sharing static document data avoids copying large macro/configuration trees for telemetry updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArcValue(#[serde(with = "arc_value")] pub Arc<Value>);
mod arc_value {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Arc<Value>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        value.as_ref().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Arc<Value>, D::Error> {
        Ok(Arc::new(Value::deserialize(deserializer)?))
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub value: Value,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metric {
    pub key: String,
    pub label: String,
    pub value: String,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub request_id: u64,
    pub ok: bool,
    pub message: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub metadata_revision: u64,
    pub version: String,
    pub machine: String,
    pub notice: String,
    pub backend_ready: bool,
    pub values: BTreeMap<String, Value>,
    pub metrics: Vec<Metric>,
    pub capabilities: Arc<BTreeMap<String, Capability>>,
    pub options: Arc<BTreeMap<String, Vec<Choice>>>,
    pub ranges: Arc<BTreeMap<String, Range>>,
    pub documents: Arc<BTreeMap<String, Document>>,
    pub collections: Arc<BTreeMap<String, Vec<Item>>>,
    pub results: VecDeque<CommandResult>,
    pub shutdown_ready: bool,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            revision: 1,
            metadata_revision: 1,
            version: env!("CARGO_PKG_VERSION").into(),
            machine: "Y7000P IRX10".into(),
            notice: "正在读取本机状态…".into(),
            backend_ready: false,
            values: BTreeMap::new(),
            metrics: Vec::new(),
            capabilities: Arc::default(),
            options: Arc::default(),
            ranges: Arc::default(),
            documents: Arc::default(),
            collections: Arc::default(),
            results: VecDeque::new(),
            shutdown_ready: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Execute {
        request_id: u64,
        command: String,
        payload: Value,
    },
    SaveDocument {
        request_id: u64,
        document: String,
        value: Value,
    },
    Shutdown {
        request_id: u64,
    },
}
impl Command {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Execute { request_id, .. }
            | Self::SaveDocument { request_id, .. }
            | Self::Shutdown { request_id } => *request_id,
        }
    }
}

#[derive(Clone)]
pub struct RuntimeHandle {
    state: Arc<Mutex<Snapshot>>,
    commands: SyncSender<Command>,
}
impl RuntimeHandle {
    pub fn report(&self, request_id: u64, ok: bool, message: String) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.notice = message.clone();
        state.results.push_back(CommandResult {
            request_id,
            ok,
            message,
        });
        while state.results.len() > 128 {
            state.results.pop_front();
        }
        state.revision = state.revision.wrapping_add(1);
    }
    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// The broker monitor still checks transport health on its own deadline,
    /// but unchanged state need not allocate/copy every live value and result.
    pub fn snapshot_if_changed(&self, observed_revision: u64) -> Option<Snapshot> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (state.revision != observed_revision).then(|| state.clone())
    }
    pub fn submit(&self, command: Command) -> Result<()> {
        self.commands
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("操作队列已满或后台已退出，请稍后重试"))
    }
}

pub fn start(data_root: PathBuf, read_only: bool) -> Result<RuntimeHandle> {
    let state = Arc::new(Mutex::new(Snapshot::default()));
    let (commands, receiver) = mpsc::sync_channel(16);
    let thread_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("jiks-runtime".into())
        .spawn(
            move || match Engine::new(data_root, read_only, thread_state.clone(), receiver) {
                Ok(mut service) => loop {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service.run())) {
                        Ok(Ok(())) => break,
                        Ok(Err(error)) => service.contain_fault(0, error),
                        Err(_) => service.contain_fault(
                            0,
                            anyhow::anyhow!("协调器异常已隔离；所有权保留，请停止并核对恢复"),
                        ),
                    }
                },
                Err(error) => {
                    let mut state = thread_state.lock().unwrap_or_else(|p| p.into_inner());
                    state.notice = format!("后台未能初始化：{error:#}");
                    let message = state.notice.clone();
                    state
                        .values
                        .insert("backend.terminal_error".into(), Value::String(message));
                    state.shutdown_ready = true;
                    state.revision = state.revision.wrapping_add(1);
                }
            },
        )?;
    Ok(RuntimeHandle { state, commands })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_poll_skips_unchanged_state_and_never_hides_results() {
        let state = Arc::new(Mutex::new(Snapshot::default()));
        let (commands, _receiver) = mpsc::sync_channel(1);
        let runtime = RuntimeHandle { state, commands };
        let initial = runtime.snapshot_if_changed(0).unwrap();
        assert!(runtime.snapshot_if_changed(initial.revision).is_none());
        runtime.report(12, false, "operation failed".into());
        let next = runtime.snapshot_if_changed(initial.revision).unwrap();
        assert_eq!(next.results.back().unwrap().request_id, 12);
        assert_eq!(next.notice, "operation failed");
        assert!(runtime.snapshot_if_changed(next.revision).is_none());
    }
    #[test]
    fn large_static_documents_are_shared_between_snapshots() {
        let mut snapshot = Snapshot::default();
        snapshot.documents = Arc::new(BTreeMap::from([(
            "macros".into(),
            Document {
                title: "Macros".into(),
                value: ArcValue(Arc::new(
                    serde_json::json!({"large": "x".repeat(1_000_000)}),
                )),
                writable: false,
                reason: String::new(),
            },
        )]));
        let copied = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot.documents, &copied.documents));
        assert!(Arc::ptr_eq(
            &snapshot.documents["macros"].value.0,
            &copied.documents["macros"].value.0
        ));
    }
}
