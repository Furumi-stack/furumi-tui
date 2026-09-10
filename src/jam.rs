//! Capability-based Jam playback control for independent federation peers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use music_dht::{ByteStream, MusicDhtService, PeerTicket, StreamAcceptor};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::app::event::AppEvent;
use crate::devices::{PlaybackCommand, PlaybackSnapshot};

pub const JAM_ALPN: &[u8] = b"furumi/jam/1";
pub const PROTOCOL_VERSION: u16 = 1;
const MAX_LINE: usize = 8 * 1024 * 1024;
const MAX_COMMANDS: usize = 128;
const PARTICIPANT_TTL_MS: i64 = 30 * 60 * 1_000;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JamRole {
    None,
    Host,
    Participant,
}

#[derive(Debug, Clone)]
pub struct JamStatus {
    pub role: JamRole,
    pub host_name: Option<String>,
    pub invite: Option<String>,
    pub participants: Vec<JamParticipant>,
    pub connected: bool,
    pub last_error: Option<String>,
}

impl Default for JamStatus {
    fn default() -> Self {
        Self {
            role: JamRole::None,
            host_name: None,
            invite: None,
            participants: Vec::new(),
            connected: false,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JamParticipant {
    pub participant_id: String,
    pub name: String,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JamInvite {
    v: u16,
    #[serde(rename = "t")]
    ticket: String,
    #[serde(rename = "j")]
    jam_id: String,
    #[serde(rename = "s")]
    secret: String,
    #[serde(rename = "d")]
    host_device_id: String,
    #[serde(rename = "n")]
    host_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JamCommand {
    command_id: String,
    participant_id: String,
    command: PlaybackCommand,
    sent_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireMessage {
    Poll {
        version: u16,
        jam_id: String,
        secret: String,
        participant: JamParticipant,
        #[serde(default)]
        commands: Vec<JamCommand>,
    },
    Snapshot {
        accepted: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        acknowledged_command_ids: Vec<String>,
        #[serde(default)]
        playback: Option<PlaybackSnapshot>,
        #[serde(default)]
        participants: Vec<JamParticipant>,
        host_time_ms: i64,
    },
    Leave {
        version: u16,
        jam_id: String,
        secret: String,
        participant_id: String,
    },
}

#[derive(Default)]
struct HostState {
    invite: Option<JamInvite>,
    invite_uri: Option<String>,
    playback: Option<PlaybackSnapshot>,
    participants: HashMap<String, JamParticipant>,
    seen_commands: HashSet<String>,
    seen_order: VecDeque<String>,
}

struct JoinedState {
    invite: JamInvite,
    participant: JamParticipant,
    pending: VecDeque<JamCommand>,
    participants: Vec<JamParticipant>,
    connected: bool,
    last_error: Option<String>,
    last_activity_ms: i64,
}

#[derive(Default)]
struct State {
    host: HostState,
    joined: Option<JoinedState>,
}

pub struct JamManager {
    state: Mutex<State>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
}

impl JamManager {
    pub fn new(event_tx: mpsc::UnboundedSender<AppEvent>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            event_tx,
        })
    }

    pub async fn create_or_regenerate(
        &self,
        service: Arc<MusicDhtService>,
        host_device_id: String,
        host_name: String,
    ) -> Result<String> {
        let invite = JamInvite {
            v: PROTOCOL_VERSION,
            ticket: service.ticket().await?.to_string(),
            jam_id: format!("jam_{}", random_hex(12)),
            secret: random_hex(32),
            host_device_id,
            host_name,
        };
        let uri = encode_invite(&invite)?;
        let mut state = lock(&self.state);
        state.joined = None;
        state.host = HostState {
            invite: Some(invite),
            invite_uri: Some(uri.clone()),
            ..HostState::default()
        };
        Ok(uri)
    }

    pub fn join(&self, uri: &str, participant_id: String, name: String) -> Result<()> {
        let invite = parse_invite(uri)?;
        let mut state = lock(&self.state);
        state.host = HostState::default();
        state.joined = Some(JoinedState {
            invite,
            participant: JamParticipant {
                participant_id,
                name,
                last_seen_ms: now_ms(),
            },
            pending: VecDeque::new(),
            participants: Vec::new(),
            connected: false,
            last_error: None,
            last_activity_ms: now_ms(),
        });
        Ok(())
    }

    pub fn leave(&self) {
        let mut state = lock(&self.state);
        state.joined = None;
        state.host = HostState::default();
    }

    pub fn status(&self) -> JamStatus {
        let state = lock(&self.state);
        if let Some(joined) = &state.joined {
            return JamStatus {
                role: JamRole::Participant,
                host_name: Some(joined.invite.host_name.clone()),
                invite: None,
                participants: joined.participants.clone(),
                connected: joined.connected,
                last_error: joined.last_error.clone(),
            };
        }
        if let Some(invite) = &state.host.invite {
            return JamStatus {
                role: JamRole::Host,
                host_name: Some(invite.host_name.clone()),
                invite: state.host.invite_uri.clone(),
                participants: state.host.participants.values().cloned().collect(),
                connected: true,
                last_error: None,
            };
        }
        JamStatus::default()
    }

    pub fn publish_host_playback(&self, snapshot: PlaybackSnapshot) {
        let mut state = lock(&self.state);
        if state.host.invite.is_some() {
            state.host.playback = Some(snapshot);
        }
    }

    pub fn submit_command(&self, command: PlaybackCommand) -> Result<()> {
        let mut state = lock(&self.state);
        let joined = state
            .joined
            .as_mut()
            .context("this player is not controlling a Jam")?;
        if joined.pending.len() >= MAX_COMMANDS {
            joined.pending.pop_front();
        }
        joined.pending.push_back(JamCommand {
            command_id: format!("cmd_{}", random_hex(16)),
            participant_id: joined.participant.participant_id.clone(),
            command,
            sent_at_ms: now_ms(),
        });
        joined.last_activity_ms = now_ms();
        Ok(())
    }

    async fn poll_once(&self, service: Arc<MusicDhtService>) -> Result<()> {
        let (invite, participant, commands) = {
            let state = lock(&self.state);
            let joined = state.joined.as_ref().context("not joined")?;
            (
                joined.invite.clone(),
                joined.participant.clone(),
                joined.pending.iter().cloned().collect::<Vec<_>>(),
            )
        };
        let ticket: PeerTicket = invite.ticket.parse().context("invalid Jam host ticket")?;
        let peer = service.connect(ticket).await?;
        let mut stream = service.open_stream(peer, JAM_ALPN).await?;
        write_message(
            &mut stream,
            &WireMessage::Poll {
                version: PROTOCOL_VERSION,
                jam_id: invite.jam_id.clone(),
                secret: invite.secret.clone(),
                participant,
                commands,
            },
        )
        .await?;
        stream.send.finish()?;
        let response = read_message(&mut stream).await?;
        let WireMessage::Snapshot {
            accepted,
            error,
            acknowledged_command_ids,
            playback,
            participants,
            ..
        } = response
        else {
            anyhow::bail!("unexpected Jam response");
        };
        anyhow::ensure!(
            accepted,
            "{}",
            error.unwrap_or_else(|| "Jam refused".into())
        );
        {
            let mut state = lock(&self.state);
            let Some(joined) = state.joined.as_mut() else {
                return Ok(());
            };
            if joined.invite.jam_id != invite.jam_id {
                return Ok(());
            }
            let acknowledged = acknowledged_command_ids.into_iter().collect::<HashSet<_>>();
            joined
                .pending
                .retain(|command| !acknowledged.contains(&command.command_id));
            joined.participants = participants;
            joined.connected = true;
            joined.last_error = None;
            joined.participant.last_seen_ms = now_ms();
            if playback
                .as_ref()
                .is_some_and(|snapshot| snapshot.state.playing && !snapshot.state.paused)
            {
                joined.last_activity_ms = now_ms();
            }
        }
        if let Some(playback) = playback {
            let _ = self.event_tx.send(AppEvent::JamPlayback(playback));
        }
        let _ = self.event_tx.send(AppEvent::JamStatus(self.status()));
        Ok(())
    }
}

pub async fn serve_peers(mut acceptor: StreamAcceptor, manager: Arc<JamManager>) {
    while let Some(stream) = acceptor.accept().await {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            if let Err(err) = serve_one(stream, manager).await {
                tracing::debug!("Jam stream failed: {err:#}");
            }
        });
    }
}

async fn serve_one(mut stream: ByteStream, manager: Arc<JamManager>) -> Result<()> {
    match read_message(&mut stream).await? {
        WireMessage::Poll {
            version,
            jam_id,
            secret,
            mut participant,
            commands,
        } => {
            let (accepted, error, acknowledged, playback, participants) = {
                let mut state = lock(&manager.state);
                let valid =
                    version == PROTOCOL_VERSION
                        && state.host.invite.as_ref().is_some_and(|invite| {
                            invite.jam_id == jam_id && invite.secret == secret
                        });
                if !valid {
                    (
                        false,
                        Some("invalid Jam capability".to_string()),
                        vec![],
                        None,
                        vec![],
                    )
                } else {
                    let now = now_ms();
                    state.host.participants.retain(|_, row| {
                        now.saturating_sub(row.last_seen_ms) <= PARTICIPANT_TTL_MS
                    });
                    participant.last_seen_ms = now;
                    state
                        .host
                        .participants
                        .insert(participant.participant_id.clone(), participant);
                    let mut acknowledged = Vec::new();
                    for command in commands.into_iter().take(MAX_COMMANDS) {
                        acknowledged.push(command.command_id.clone());
                        if state.host.seen_commands.insert(command.command_id.clone()) {
                            state.host.seen_order.push_back(command.command_id.clone());
                            let _ = manager.event_tx.send(AppEvent::JamCommand(command.command));
                        }
                    }
                    while state.host.seen_order.len() > 4096 {
                        if let Some(id) = state.host.seen_order.pop_front() {
                            state.host.seen_commands.remove(&id);
                        }
                    }
                    (
                        true,
                        None,
                        acknowledged,
                        state.host.playback.clone(),
                        state.host.participants.values().cloned().collect(),
                    )
                }
            };
            write_message(
                &mut stream,
                &WireMessage::Snapshot {
                    accepted,
                    error,
                    acknowledged_command_ids: acknowledged,
                    playback,
                    participants,
                    host_time_ms: now_ms(),
                },
            )
            .await?;
            stream.send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.send.stopped()).await;
            let _ = manager.event_tx.send(AppEvent::JamStatus(manager.status()));
        }
        WireMessage::Leave {
            version,
            jam_id,
            secret,
            participant_id,
        } => {
            let mut state = lock(&manager.state);
            if version == PROTOCOL_VERSION
                && state
                    .host
                    .invite
                    .as_ref()
                    .is_some_and(|invite| invite.jam_id == jam_id && invite.secret == secret)
            {
                state.host.participants.remove(&participant_id);
            }
            drop(state);
            let _ = manager.event_tx.send(AppEvent::JamStatus(manager.status()));
        }
        WireMessage::Snapshot { .. } => anyhow::bail!("unexpected Jam snapshot"),
    }
    Ok(())
}

pub async fn poll_loop(manager: Arc<JamManager>, service: Arc<MusicDhtService>) {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;
        if manager.status().role != JamRole::Participant {
            continue;
        }
        let expired = {
            let state = lock(&manager.state);
            state.joined.as_ref().is_some_and(|joined| {
                now_ms().saturating_sub(joined.last_activity_ms) > PARTICIPANT_TTL_MS
            })
        };
        if expired {
            manager.leave();
            let _ = manager.event_tx.send(AppEvent::JamStatus(manager.status()));
            let _ = manager.event_tx.send(AppEvent::StatusMessage(
                "Jam ended after 30 minutes without playback or commands".into(),
            ));
            continue;
        }
        if let Err(err) = manager.poll_once(Arc::clone(&service)).await {
            {
                let mut state = lock(&manager.state);
                if let Some(joined) = state.joined.as_mut() {
                    joined.connected = false;
                    joined.last_error = Some(format!("{err:#}"));
                }
            }
            let _ = manager.event_tx.send(AppEvent::JamStatus(manager.status()));
        }
    }
}

async fn write_message(stream: &mut ByteStream, message: &WireMessage) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    anyhow::ensure!(bytes.len() <= MAX_LINE, "Jam message is too large");
    bytes.push(b'\n');
    stream.send.write_all(&bytes).await?;
    Ok(())
}

async fn read_message(stream: &mut ByteStream) -> Result<WireMessage> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = stream.recv.read(&mut byte).await?;
        if read.unwrap_or(0) == 0 || byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
        anyhow::ensure!(bytes.len() <= MAX_LINE, "Jam message is too large");
    }
    anyhow::ensure!(!bytes.is_empty(), "empty Jam message");
    Ok(serde_json::from_slice(&bytes)?)
}

fn encode_invite(invite: &JamInvite) -> Result<String> {
    Ok(format!(
        "frid://j/{}",
        base64url_encode(&serde_json::to_vec(invite)?)
    ))
}

fn parse_invite(uri: &str) -> Result<JamInvite> {
    let encoded = uri
        .trim()
        .strip_prefix("frid://j/")
        .context("expected frid://j invite")?;
    let invite: JamInvite = serde_json::from_slice(&base64url_decode(encoded)?)?;
    anyhow::ensure!(
        invite.v == PROTOCOL_VERSION,
        "unsupported Jam invite version"
    );
    anyhow::ensure!(
        !invite.ticket.is_empty()
            && !invite.jam_id.is_empty()
            && invite.secret.len() >= 16
            && !invite.host_device_id.is_empty(),
        "incomplete Jam invite"
    );
    Ok(invite)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seed = format!(
        "{}:{}:{}",
        now_ms(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let hash = blake3::hash(seed.as_bytes()).to_hex().to_string();
    hash[..(bytes * 2).min(hash.len())].to_string()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(value: &str) -> Result<Vec<u8>> {
    fn decode(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    anyhow::ensure!(bytes.len() % 4 != 1, "invalid base64url Jam invite");
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i < bytes.len() {
        let a = decode(bytes[i]).context("invalid base64url Jam invite")?;
        let b = decode(*bytes.get(i + 1).context("truncated Jam invite")?)
            .context("invalid base64url Jam invite")?;
        let c = bytes.get(i + 2).and_then(|byte| decode(*byte));
        let d = bytes.get(i + 3).and_then(|byte| decode(*byte));
        out.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            out.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                out.push((c << 6) | d);
            }
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_round_trips_and_is_separate_from_pairing() {
        let invite = JamInvite {
            v: PROTOCOL_VERSION,
            ticket: "ticket".into(),
            jam_id: "jam_test".into(),
            secret: "0123456789abcdef".into(),
            host_device_id: "dev_host".into(),
            host_name: "Host".into(),
        };
        let uri = encode_invite(&invite).unwrap();
        assert!(uri.starts_with("frid://j/"));
        assert_eq!(parse_invite(&uri).unwrap().jam_id, "jam_test");
        assert!(parse_invite("frid://i/abcd").is_err());
    }

    #[tokio::test]
    async fn participant_receives_host_state_and_host_receives_command() {
        let unique = random_hex(8);
        let host_dir = std::env::temp_dir().join(format!("furumi-jam-host-{unique}"));
        let guest_dir = std::env::temp_dir().join(format!("furumi-jam-guest-{unique}"));
        std::fs::create_dir_all(&host_dir).unwrap();
        std::fs::create_dir_all(&guest_dir).unwrap();
        let network = music_dht::NetworkId::from_name(&format!("jam-test-{unique}"));
        let host_config = music_dht::MusicDhtConfig::builder()
            .data_dir(&host_dir)
            .network_id(network)
            .stream_protocol(JAM_ALPN)
            .build()
            .unwrap();
        let guest_config = music_dht::MusicDhtConfig::builder()
            .data_dir(&guest_dir)
            .network_id(network)
            .stream_protocol(JAM_ALPN)
            .build()
            .unwrap();
        let (host_service, mut host_events) = music_dht::MusicDhtService::start(host_config)
            .await
            .unwrap();
        let (guest_service, mut guest_events) = music_dht::MusicDhtService::start(guest_config)
            .await
            .unwrap();
        let host_service = Arc::new(host_service);
        let guest_service = Arc::new(guest_service);
        let host_drain = tokio::spawn(async move { while host_events.recv().await.is_some() {} });
        let guest_drain = tokio::spawn(async move { while guest_events.recv().await.is_some() {} });

        let (host_tx, mut host_rx) = mpsc::unbounded_channel();
        let (guest_tx, mut guest_rx) = mpsc::unbounded_channel();
        let host = JamManager::new(host_tx);
        let guest = JamManager::new(guest_tx);
        let invite = host
            .create_or_regenerate(Arc::clone(&host_service), "dev_host".into(), "Host".into())
            .await
            .unwrap();
        guest
            .join(&invite, "dev_guest".into(), "Guest".into())
            .unwrap();

        let playback_state = crate::devices::PlaybackStateWire {
            queue: Vec::new(),
            queue_pos: 0,
            playing: false,
            paused: false,
            idle_since_ms: Some(now_ms()),
            position_secs: 0.0,
            volume: 73,
            shuffle: false,
            repeat: crate::devices::PlaybackRepeat::Off,
        };
        host.publish_host_playback(PlaybackSnapshot {
            coordination: None,
            device_id: "dev_host".into(),
            device_name: "Host".into(),
            active: true,
            updated_at_ms: now_ms(),
            state: playback_state.clone(),
        });
        let acceptor = host_service.stream_acceptor(JAM_ALPN).unwrap();
        let server = tokio::spawn(serve_peers(acceptor, Arc::clone(&host)));

        guest.poll_once(Arc::clone(&guest_service)).await.unwrap();
        let playback = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(AppEvent::JamPlayback(snapshot)) = guest_rx.recv().await {
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(playback.device_id, "dev_host");
        assert_eq!(playback.state.volume, 73);

        guest
            .submit_command(PlaybackCommand::SetState {
                state: crate::devices::PlaybackStateWire {
                    paused: true,
                    ..playback_state
                },
                seek: false,
            })
            .unwrap();
        guest.poll_once(Arc::clone(&guest_service)).await.unwrap();
        let command = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(AppEvent::JamCommand(command)) = host_rx.recv().await {
                    break command;
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            command,
            PlaybackCommand::SetState {
                state: crate::devices::PlaybackStateWire { paused: true, .. },
                seek: false
            }
        ));

        server.abort();
        host_drain.abort();
        guest_drain.abort();
        host_service.shutdown().await.unwrap();
        guest_service.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(host_dir);
        let _ = std::fs::remove_dir_all(guest_dir);
    }
}
