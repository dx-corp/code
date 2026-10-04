//! Process-local observation/control of authenticated native coding turns.
//! Session JSONL remains the transcript owner. This registry neither retries
//! effects nor restores execution after a gateway restart. Accepted identities
//! are retained for a bounded observation window; reattachment is a read only.
use super::*;
use crate::auth::AuthSource;
use crate::chat_output::NativeTurnOutput;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const MAX_TURNS: usize = 128;
const MAX_QUEUED: usize = 32;
pub(crate) const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_RETAINED_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_ACCEPTED_IDENTITIES: usize = 4096;
const MAX_REPLAY_EVENTS: usize = 256;
const MAX_REPLAY_BYTES: usize = 256 * 1024;
const MAX_PROJECTION_BYTES: usize = 1024 * 1024;
const MAX_PROJECTION_EVENTS: usize = 256;
const MAX_APPROVAL_PROJECTION_BYTES: usize = 256 * 1024;
const MAX_APPROVAL_PROJECTIONS: usize = 32;
const RETENTION: Duration = Duration::from_secs(30 * 60);
const EXECUTION_LEASE: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TurnBinding {
    session_id: String,
    session_created_at: String,
    subject: Option<String>,
    organization_id: Option<String>,
    workspace_id: Option<String>,
    source: AuthSource,
    cwd: PathBuf,
}

// Execution ordering belongs to the session generation, independently of
// which authenticated principal observes or accepts a prompt in that session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SessionLane {
    session_id: String,
    session_created_at: String,
    cwd: PathBuf,
}
impl TurnBinding {
    fn lane(&self) -> SessionLane {
        SessionLane {
            session_id: self.session_id.clone(),
            session_created_at: self.session_created_at.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum TurnState {
    Queued,
    Running,
    Stopping,
    Completed,
    Failed,
    Cancelled,
}
impl TurnState {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

pub(crate) enum NativeTurnCommand {
    Steer {
        content: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

struct TurnData {
    generation: u64,
    state: TurnState,
    request: ChatRequest,
    sequence: u64,
    replay: VecDeque<(u64, Value, usize)>,
    replay_bytes: usize,
    message: Option<Value>,
    projection: VecDeque<Value>,
    projection_truncated: bool,
    approvals: HashMap<String, Value>,
    error: Option<String>,
    finished_at: Option<Instant>,
}

pub(crate) struct NativeTurn {
    id: String,
    binding: TurnBinding,
    original_request: String,
    auth: AuthContext,
    steering_supported: bool,
    accepted_at: Instant,
    data: StdMutex<TurnData>,
    pub(crate) cancel: CancellationToken,
    commands: mpsc::Sender<NativeTurnCommand>,
    command_receiver: StdMutex<Option<mpsc::Receiver<NativeTurnCommand>>>,
}

impl NativeTurn {
    pub(crate) fn take_commands(&self) -> Option<mpsc::Receiver<NativeTurnCommand>> {
        self.command_receiver
            .lock()
            .expect("native command lock")
            .take()
    }

    pub(crate) fn publish(&self, mut event: Value) {
        let mut data = self.data.lock().expect("native turn lock");
        if data.state.terminal() {
            return;
        }
        data.sequence += 1;
        let kind = event["type"].as_str().unwrap_or_default().to_owned();
        // Existing error producers omit classification. Only an explicitly
        // recoverable actor error may leave the accepted turn successful.
        let terminal_error = kind == "error"
            && !(event["fatal"].as_bool() == Some(false)
                && event["terminal"].as_bool() == Some(false));
        let encoded_len = event.to_string().len();
        if encoded_len > MAX_PROJECTION_BYTES {
            // The transcript stays authoritative. Never retain an unbounded
            // provider event or pretend a truncated projection is complete.
            data.projection_truncated = true;
            if matches!(
                kind.as_str(),
                "message_start" | "message_update" | "message_end"
            ) {
                data.projection
                    .retain(|prior| projection_key(prior).as_deref() != Some("message"));
                if let Some(message) = event.get("message") {
                    let excerpt = crate::sessions::bounded_public_session_message(message);
                    data.message = Some(excerpt.clone());
                    event = serde_json::json!({"type":kind,"message":excerpt,"projectionTruncated":true});
                } else {
                    data.message = None;
                    event =
                        serde_json::json!({"type":"desktop_projection_truncated","eventType":kind});
                }
            } else {
                event = serde_json::json!({"type":"desktop_projection_truncated","eventType":kind,"message":"Event exceeds the bounded replay projection"});
            }
        }
        if matches!(
            kind.as_str(),
            "message_start" | "message_update" | "message_end"
        ) {
            if let Some(message) = event.get("message") {
                data.message = Some(message.clone());
            }
        }
        if terminal_error {
            data.error = Some(
                event["message"]
                    .as_str()
                    .unwrap_or("Turn failed")
                    .to_owned(),
            );
        }
        if kind == "action_approval_required" {
            if let Some(id) = event["request"]["id"].as_str() {
                let retained = data
                    .approvals
                    .values()
                    .map(|value| value.to_string().len())
                    .sum::<usize>();
                if data.approvals.len() < MAX_APPROVAL_PROJECTIONS
                    && retained.saturating_add(event.to_string().len())
                        <= MAX_APPROVAL_PROJECTION_BYTES
                {
                    data.approvals.insert(id.to_owned(), event.clone());
                } else {
                    data.projection_truncated = true;
                }
            }
        }
        if kind == "tool_execution_end" {
            if let Some(id) = event["toolCallId"].as_str() {
                data.approvals.remove(id);
            }
        }
        // Latest message/status plus per-tool lifecycle and receipts reconstruct
        // presentation when the sequence ring has rolled over. Approval channels
        // remain in the existing native approval owner, never this projection.
        let key = projection_key(&event);
        if let Some(key) = &key {
            data.projection
                .retain(|prior| projection_key(prior).as_ref() != Some(key));
        }
        if key.is_some() {
            data.projection.push_back(event.clone());
        }
        while data.projection.len() > MAX_PROJECTION_EVENTS
            || data
                .projection
                .iter()
                .map(|value| value.to_string().len())
                .sum::<usize>()
                > MAX_PROJECTION_BYTES
        {
            data.projection.pop_front();
            data.projection_truncated = true;
        }
        let size = event.to_string().len();
        let sequence = data.sequence;
        if size <= MAX_REPLAY_BYTES {
            data.replay.push_back((sequence, event, size));
            data.replay_bytes += size;
        } else {
            data.replay.clear();
            data.replay_bytes = 0;
        }
        while data.replay.len() > MAX_REPLAY_EVENTS || data.replay_bytes > MAX_REPLAY_BYTES {
            if let Some((_, _, bytes)) = data.replay.pop_front() {
                data.replay_bytes -= bytes;
            }
        }
        if kind == "done" {
            data.state = if self.cancel.is_cancelled() {
                TurnState::Cancelled
            } else if data.error.is_some() {
                TurnState::Failed
            } else {
                TurnState::Completed
            };
            data.generation += 1;
            data.finished_at = Some(Instant::now());
            data.approvals.clear();
        }
    }

    fn snapshot(&self, epoch: &str, after: u64, queued: Vec<Value>) -> Value {
        let data = self.data.lock().expect("native turn lock");
        let oldest = data
            .replay
            .front()
            .map(|entry| entry.0)
            .unwrap_or(data.sequence + 1);
        let reset = after > data.sequence || after.saturating_add(1) < oldest;
        serde_json::json!({
            "gatewayEpoch":epoch,"turnId":self.id,"sessionId":self.binding.session_id,
            "sessionCreatedAt":self.binding.session_created_at,"generation":data.generation,
            "state":data.state,"sequence":data.sequence,"resetRequired":reset,
            "events":if reset { Vec::<Value>::new() } else { data.replay.iter().filter(|entry| entry.0 > after).map(|(sequence,event,_)| serde_json::json!({"sequence":sequence,"event":event})).collect() },
            "message":data.message,"projectionEvents":data.projection,"projectionTruncated":data.projection_truncated,
            "pendingApprovals":data.approvals.values().collect::<Vec<_>>(),"error":data.error,
            "prompt":latest_prompt(&data.request),"steeringSupported":self.steering_supported,"queued":queued
        })
    }

    fn edit(&self, generation: u64, request: ChatRequest) -> Result<(), TurnError> {
        let mut data = self.data.lock().expect("native turn lock");
        check_generation(&data, generation)?;
        if data.state != TurnState::Queued {
            return Err(TurnError::conflict("Only queued prompts can be edited"));
        }
        data.request = request;
        data.generation += 1;
        Ok(())
    }
    fn remove(&self, generation: u64) -> Result<(), TurnError> {
        let mut data = self.data.lock().expect("native turn lock");
        check_generation(&data, generation)?;
        if data.state != TurnState::Queued {
            return Err(TurnError::conflict("Only queued prompts can be removed"));
        }
        data.state = TurnState::Cancelled;
        data.generation += 1;
        data.finished_at = Some(Instant::now());
        Ok(())
    }
    fn stop(&self, generation: u64) -> Result<(), TurnError> {
        let mut data = self.data.lock().expect("native turn lock");
        check_generation(&data, generation)?;
        if !matches!(data.state, TurnState::Running | TurnState::Stopping) {
            return Err(TurnError::conflict("Only an active turn can be stopped"));
        }
        data.state = TurnState::Stopping;
        data.generation += 1;
        self.cancel.cancel();
        Ok(())
    }
    async fn steer(&self, generation: u64, content: String) -> Result<(), TurnError> {
        if content.trim().is_empty() || content.len() > 64 * 1024 {
            return Err(TurnError::bad("Steering requires bounded non-empty text"));
        }
        if !self.steering_supported {
            return Err(TurnError {
                status: 422,
                message: "This provider does not support steering an active turn".into(),
            });
        }
        let (reply, receive) = oneshot::channel();
        {
            let mut data = self.data.lock().expect("native turn lock");
            check_generation(&data, generation)?;
            if data.state != TurnState::Running {
                return Err(TurnError::conflict("Steering requires a running turn"));
            }
            self.commands
                .try_send(NativeTurnCommand::Steer { content, reply })
                .map_err(|_| {
                    TurnError::conflict("Native steering channel is unavailable or full")
                })?;
            data.generation += 1;
        }
        match tokio::time::timeout(Duration::from_secs(15), receive).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(TurnError::conflict(&message)),
            _ => Err(TurnError::conflict(
                "Steering acknowledgement unavailable; read the turn before another control",
            )),
        }
    }
}

fn projection_key(event: &Value) -> Option<String> {
    let kind = event["type"].as_str()?;
    match kind {
        "message_start" | "message_update" | "message_end" => Some("message".into()),
        "action_approval_required" => event["request"]["id"]
            .as_str()
            .map(|id| format!("approval:{id}")),
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            event["toolCallId"]
                .as_str()
                .map(|id| format!("{kind}:{id}"))
        }
        "status" if event["status"] == "managed_gateway_receipt" => event["details"]["requestId"]
            .as_str()
            .map(|id| format!("receipt:{id}")),
        "status" | "error" | "done" | "agent_start" | "agent_end" | "turn_start" | "turn_end"
        | "compaction" => Some(kind.into()),
        _ => None,
    }
}
fn latest_prompt(request: &ChatRequest) -> String {
    request
        .messages
        .last()
        .map(|message| crate::chat::composer_text_content(&message.content))
        .unwrap_or_default()
}
fn request_identity(request: &ChatRequest) -> String {
    let tools = request.tools.iter().map(|tool| serde_json::json!({"name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect::<Vec<_>>();
    serde_json::json!({"model":request.model,"thinkingLevel":request.thinking_level,"sessionId":request.session_id,"messages":request.messages,"tools":tools}).to_string()
}
fn check_generation(data: &TurnData, generation: u64) -> Result<(), TurnError> {
    if generation == data.generation {
        Ok(())
    } else {
        Err(TurnError::conflict(
            "Turn changed; read the current owner generation before controlling it",
        ))
    }
}

#[derive(Default)]
struct Entries {
    turns: Vec<Arc<NativeTurn>>,
    lanes: HashSet<SessionLane>,
    accepted: HashMap<String, TurnBinding>,
}
pub(super) struct NativeTurnRuntime {
    epoch: String,
    entries: StdMutex<Entries>,
    shutdown: CancellationToken,
}
impl Default for NativeTurnRuntime {
    fn default() -> Self {
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).expect("native turn epoch randomness");
        use base64::Engine;
        Self {
            epoch: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy),
            entries: StdMutex::new(Entries::default()),
            shutdown: CancellationToken::new(),
        }
    }
}
struct AcceptedTurn {
    turn: Arc<NativeTurn>,
    start_lane: bool,
}

impl NativeTurnRuntime {
    fn accept(
        &self,
        binding: TurnBinding,
        id: String,
        mut request: ChatRequest,
        auth: AuthContext,
        steering_supported: bool,
        resolved_model: Option<String>,
    ) -> Result<AcceptedTurn, TurnError> {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let original_request = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(request_identity(&request).as_bytes()));
        let mut entries = self.entries.lock().expect("native registry lock");
        // Look up before pruning. A retained retry always rejoins its exact accepted identity.
        if let Some(turn) = entries.turns.iter().find(|turn| turn.id == id) {
            if turn.binding != binding {
                return Err(TurnError::not_found());
            }
            if turn.original_request != original_request {
                return Err(TurnError::conflict(
                    "Turn identity was already accepted with different input",
                ));
            }
            return Ok(AcceptedTurn {
                turn: turn.clone(),
                start_lane: false,
            });
        }
        // Bounded lifetime tombstones prevent an expired accepted ID from
        // starting its effects again. Capacity exhaustion fails explicitly.
        if entries.accepted.contains_key(&id) {
            return Err(TurnError::conflict(
                "Accepted turn observation expired; this identity cannot execute again",
            ));
        }
        if entries.accepted.len() >= MAX_ACCEPTED_IDENTITIES {
            return Err(TurnError {
                status: 429,
                message: "Native accepted identity capacity exhausted".into(),
            });
        }
        entries.turns.retain(|turn| {
            turn.data
                .lock()
                .expect("native turn lock")
                .finished_at
                .is_none_or(|at| at.elapsed() <= RETENTION)
        });
        if entries.turns.len() >= MAX_TURNS {
            return Err(TurnError {status:429,message:"Native turn observation capacity is full; retained identities cannot be evicted safely".into()});
        }
        if entries
            .turns
            .iter()
            .filter(|turn| {
                turn.binding.lane() == binding.lane()
                    && turn.data.lock().expect("native turn lock").state == TurnState::Queued
            })
            .count()
            >= MAX_QUEUED
        {
            return Err(TurnError {
                status: 429,
                message: "Session prompt queue is full".into(),
            });
        }
        if request.model.is_none() {
            request.model = resolved_model;
        }
        let retained_bytes = entries
            .turns
            .iter()
            .map(|turn| {
                request_identity(&turn.data.lock().expect("native turn lock").request).len()
            })
            .sum::<usize>();
        if retained_bytes.saturating_add(request_identity(&request).len())
            > MAX_RETAINED_REQUEST_BYTES
        {
            return Err(TurnError {
                status: 429,
                message: "Native retained prompt byte capacity is full".into(),
            });
        }
        entries.accepted.insert(id.clone(), binding.clone());
        let (commands, receiver) = mpsc::channel(32);
        let turn = Arc::new(NativeTurn {
            id,
            binding: binding.clone(),
            original_request,
            auth,
            steering_supported,
            accepted_at: Instant::now(),
            cancel: self.shutdown.child_token(),
            commands,
            command_receiver: StdMutex::new(Some(receiver)),
            data: StdMutex::new(TurnData {
                generation: 1,
                state: TurnState::Queued,
                request,
                sequence: 0,
                replay: VecDeque::new(),
                replay_bytes: 0,
                message: None,
                projection: VecDeque::new(),
                projection_truncated: false,
                approvals: HashMap::new(),
                error: None,
                finished_at: None,
            }),
        });
        entries.turns.push(turn.clone());
        let start_lane = entries.lanes.insert(binding.lane());
        Ok(AcceptedTurn { turn, start_lane })
    }
    fn edit_turn(
        &self,
        turn: &NativeTurn,
        generation: u64,
        request: ChatRequest,
    ) -> Result<(), TurnError> {
        let entries = self.entries.lock().expect("native registry lock");
        let retained_bytes = entries
            .turns
            .iter()
            .filter(|other| other.id != turn.id)
            .map(|other| {
                request_identity(&other.data.lock().expect("native turn lock").request).len()
            })
            .sum::<usize>();
        if retained_bytes.saturating_add(request_identity(&request).len())
            > MAX_RETAINED_REQUEST_BYTES
        {
            return Err(TurnError {
                status: 429,
                message: "Native retained prompt byte capacity is full".into(),
            });
        }
        turn.edit(generation, request)
    }
    fn find(&self, binding: &TurnBinding, id: &str) -> Option<Arc<NativeTurn>> {
        self.entries
            .lock()
            .expect("native registry lock")
            .turns
            .iter()
            .find(|turn| turn.id == id && &turn.binding == binding)
            .filter(|turn| {
                turn.data
                    .lock()
                    .expect("native turn lock")
                    .finished_at
                    .is_none_or(|at| at.elapsed() <= RETENTION)
            })
            .cloned()
    }
    fn next(&self, lane: &SessionLane) -> Option<Arc<NativeTurn>> {
        let mut entries = self.entries.lock().expect("native registry lock");
        for turn in entries
            .turns
            .iter()
            .filter(|turn| turn.binding.lane() == *lane)
        {
            let mut data = turn.data.lock().expect("native turn lock");
            if data.state == TurnState::Queued {
                if turn.accepted_at.elapsed() > EXECUTION_LEASE {
                    data.state = TurnState::Failed;
                    data.error = Some("Queued prompt expired before execution".into());
                    data.finished_at = Some(Instant::now());
                    data.generation += 1;
                    continue;
                }
                data.state = TurnState::Running;
                data.generation += 1;
                return Some(turn.clone());
            }
        }
        entries.lanes.remove(lane);
        None
    }
    fn queue(&self, binding: &TurnBinding) -> Vec<Value> {
        self.entries.lock().expect("native registry lock").turns.iter().filter(|turn| &turn.binding == binding).filter_map(|turn| {
            let data = turn.data.lock().expect("native turn lock");
            (data.state == TurnState::Queued).then(|| {
                let prompt=latest_prompt(&data.request);
                let excerpt=prompt.chars().take(1024).collect::<String>();
                serde_json::json!({"turnId":turn.id,"generation":data.generation,"promptTruncated":excerpt.len()<prompt.len(),"prompt":excerpt,"state":"queued"})
            })
        }).collect()
    }
    fn snapshots(&self, binding: &TurnBinding) -> Vec<Value> {
        let turns = self
            .entries
            .lock()
            .expect("native registry lock")
            .turns
            .iter()
            .filter(|turn| &turn.binding == binding)
            .filter(|turn| {
                turn.data
                    .lock()
                    .expect("native turn lock")
                    .finished_at
                    .is_none_or(|at| at.elapsed() <= RETENTION)
            })
            .cloned()
            .collect::<Vec<_>>();
        // The roster contains one foreground owner and one bounded queue.
        // Retained completed projections are read explicitly by turn identity.
        let foreground = turns
            .iter()
            .find(|turn| {
                matches!(
                    turn.data.lock().expect("native turn lock").state,
                    TurnState::Running | TurnState::Stopping
                )
            })
            .or_else(|| {
                turns.iter().find(|turn| {
                    turn.data.lock().expect("native turn lock").state == TurnState::Queued
                })
            })
            .or_else(|| turns.last());
        foreground
            .map(|turn| vec![turn.snapshot(&self.epoch, 0, self.queue(binding))])
            .unwrap_or_default()
    }
    pub(super) fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

struct TurnError {
    status: u16,
    message: String,
}
impl TurnError {
    fn bad(message: &str) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }
    fn conflict(message: &str) -> Self {
        Self {
            status: 409,
            message: message.into(),
        }
    }
    fn not_found() -> Self {
        Self {
            status: 404,
            message: "Accepted turn or session generation unavailable; no execution was resumed"
                .into(),
        }
    }
    fn response(self) -> Vec<u8> {
        json_response(self.status, &serde_json::json!({"error":self.message}))
    }
}
impl std::fmt::Debug for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnError")
            .field("status", &self.status)
            .field("message", &self.message)
            .finish()
    }
}

#[path = "native_turns_control.rs"]
mod control;
#[cfg(test)]
use control::execution_request;
pub(crate) use control::{handle_native_turn_endpoint, is_native_turn_endpoint};

pub(crate) async fn session_has_active_native_turn(
    state: &AppState,
    id: &str,
    created_at: &str,
) -> bool {
    state
        .native_turns
        .entries
        .lock()
        .expect("native registry lock")
        .turns
        .iter()
        .any(|turn| {
            turn.binding.session_id == id
                && turn.binding.session_created_at == created_at
                && !turn.data.lock().expect("native turn lock").state.terminal()
        })
}

#[cfg(test)]
#[path = "native_turns_tests.rs"]
mod native_turns_tests;
