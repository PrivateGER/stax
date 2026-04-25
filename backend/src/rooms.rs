use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::extract::ws::{Message, WebSocket};
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::{
    sync::{Mutex as TokioMutex, RwLock, broadcast, mpsc},
    task::JoinHandle,
};
use tracing::warn;
use uuid::Uuid;

use crate::{
    ROOM_EVENT_BUFFER,
    clock::{AuthoritativePlaybackClock, format_timestamp, round_to},
    library::LibraryService,
    persistence::Persistence,
    protocol::{ClientSocketMessage, Participant, PlaybackAction, Room, ServerEvent},
};

pub(crate) type SharedRooms = Arc<RwLock<HashMap<Uuid, SharedRoom>>>;
pub(crate) type SharedRoom = Arc<RoomHub>;

#[derive(Debug)]
pub(crate) struct RoomHub {
    room: RwLock<RoomRecord>,
    persistence: Persistence,
    library: LibraryService,
    connection_count: AtomicUsize,
    participants: RwLock<HashMap<Uuid, Participant>>,
    events: broadcast::Sender<ServerEvent>,
    cleanup_tx: mpsc::UnboundedSender<Uuid>,
    cleanup_task: TokioMutex<Option<JoinHandle<()>>>,
    empty_room_grace: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct RoomRecord {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) media_id: Option<Uuid>,
    pub(crate) media_title: Option<String>,
    pub(crate) created_at: String,
    pub(crate) clock: AuthoritativePlaybackClock,
}

pub(crate) enum SocketDispatch {
    Broadcast(ServerEvent),
    Direct(ServerEvent),
}

impl RoomHub {
    pub(crate) fn new(
        room: RoomRecord,
        persistence: Persistence,
        library: LibraryService,
        cleanup_tx: mpsc::UnboundedSender<Uuid>,
        empty_room_grace: Duration,
    ) -> Self {
        let (events, _) = broadcast::channel(ROOM_EVENT_BUFFER);

        Self {
            room: RwLock::new(room),
            persistence,
            library,
            connection_count: AtomicUsize::new(0),
            participants: RwLock::new(HashMap::new()),
            events,
            cleanup_tx,
            cleanup_task: TokioMutex::new(None),
            empty_room_grace,
        }
    }

    pub(crate) async fn participant_list(&self) -> Vec<Participant> {
        let guard = self.participants.read().await;
        let mut list: Vec<Participant> = guard.values().cloned().collect();
        list.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        list
    }

    pub(crate) async fn add_participant(&self, id: Uuid, name: String) {
        let mut guard = self.participants.write().await;
        guard.insert(
            id,
            Participant {
                id,
                name,
                drift_seconds: None,
            },
        );
    }

    async fn remove_participant(&self, id: Uuid) {
        let mut guard = self.participants.write().await;
        guard.remove(&id);
    }

    async fn update_participant_drift(&self, id: Uuid, drift_seconds: f64) {
        let mut guard = self.participants.write().await;
        if let Some(entry) = guard.get_mut(&id) {
            entry.drift_seconds = Some(drift_seconds);
        }
    }

    pub(crate) async fn snapshot(&self) -> Room {
        self.room.read().await.snapshot(OffsetDateTime::now_utc())
    }

    fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    pub(crate) fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::Relaxed)
    }

    async fn join(self: &Arc<Self>) -> usize {
        let count = self.connection_count.fetch_add(1, Ordering::Relaxed) + 1;
        // A join cancels any pending empty-room cleanup — somebody's back
        // before the grace period expired.
        if let Some(handle) = self.cleanup_task.lock().await.take() {
            handle.abort();
        }
        count
    }

    async fn leave(self: &Arc<Self>) -> usize {
        let previous = self.connection_count.fetch_sub(1, Ordering::Relaxed);
        let count = previous.saturating_sub(1);
        if count == 0 {
            self.schedule_cleanup().await;
        }
        count
    }

    pub(crate) async fn schedule_cleanup(self: &Arc<Self>) {
        let mut guard = self.cleanup_task.lock().await;
        if let Some(handle) = guard.take() {
            handle.abort();
        }
        let room_id = self.room.read().await.id;
        let tx = self.cleanup_tx.clone();
        let grace = self.empty_room_grace;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            // The receiver task double-checks connection_count before
            // deleting, so a race with a late join after this send is
            // harmless.
            let _ = tx.send(room_id);
        });
        *guard = Some(handle);
    }

    fn broadcast(&self, event: ServerEvent) {
        let _ = self.events.send(event);
    }

    pub(crate) async fn apply_socket_message(
        &self,
        command: ClientSocketMessage,
        actor: &str,
        connection_id: Uuid,
    ) -> Result<SocketDispatch, &'static str> {
        // SelectMedia needs to resolve the incoming media id against the
        // library before touching room state, so it owns its own write-lock
        // path instead of sharing the clone/swap pattern below.
        if let ClientSocketMessage::SelectMedia { media_id } = command {
            return self.apply_select_media(media_id, actor).await;
        }

        let now = OffsetDateTime::now_utc();
        let mut room = self.room.write().await;
        let mut updated_room = room.clone();

        let dispatch = match command {
            ClientSocketMessage::Play {
                position_seconds,
                client_one_way_ms,
            } => {
                let anchor_time = back_date_for_client_latency(now, client_one_way_ms);
                updated_room.clock.play(
                    anchor_time,
                    position_seconds
                        .map(normalize_command_position)
                        .transpose()?,
                );

                SocketDispatch::Broadcast(ServerEvent::PlaybackUpdated {
                    room: updated_room.snapshot(now),
                    actor: actor.to_string(),
                    action: PlaybackAction::Play,
                })
            }
            ClientSocketMessage::Pause {
                position_seconds,
                client_one_way_ms,
            } => {
                let anchor_time = back_date_for_client_latency(now, client_one_way_ms);
                updated_room.clock.pause(
                    anchor_time,
                    position_seconds
                        .map(normalize_command_position)
                        .transpose()?,
                );

                SocketDispatch::Broadcast(ServerEvent::PlaybackUpdated {
                    room: updated_room.snapshot(now),
                    actor: actor.to_string(),
                    action: PlaybackAction::Pause,
                })
            }
            ClientSocketMessage::Seek {
                position_seconds,
                client_one_way_ms,
            } => {
                let anchor_time = back_date_for_client_latency(now, client_one_way_ms);
                updated_room
                    .clock
                    .seek(anchor_time, normalize_command_position(position_seconds)?);

                SocketDispatch::Broadcast(ServerEvent::PlaybackUpdated {
                    room: updated_room.snapshot(now),
                    actor: actor.to_string(),
                    action: PlaybackAction::Seek,
                })
            }
            ClientSocketMessage::SelectMedia { .. } => {
                unreachable!("SelectMedia is handled before the main match");
            }
            ClientSocketMessage::ReportPosition { position_seconds } => {
                let reported_position_seconds = normalize_reported_position(position_seconds)?;
                let drift = room.clock.report_drift(now, reported_position_seconds);
                let room_id = room.id;
                drop(room);

                self.update_participant_drift(connection_id, drift.delta_seconds)
                    .await;
                self.broadcast(ServerEvent::ParticipantsUpdated {
                    room_id,
                    participants: self.participant_list().await,
                });

                return Ok(SocketDispatch::Direct(ServerEvent::DriftCorrection {
                    room_id,
                    actor: actor.to_string(),
                    reported_position_seconds: drift.reported_position_seconds,
                    expected_position_seconds: drift.expected_position_seconds,
                    delta_seconds: drift.delta_seconds,
                    tolerance_seconds: drift.tolerance_seconds,
                    suggested_action: drift.suggested_action,
                    measured_at: format_timestamp(now),
                }));
            }
            ClientSocketMessage::Ping { client_sent_at_ms } => {
                return Ok(SocketDispatch::Direct(ServerEvent::Pong {
                    client_sent_at_ms,
                }));
            }
        };

        if let Err(error) = self.persistence.save_room(&updated_room).await {
            warn!(%error, room_id = %updated_room.id, "failed to persist room state");
            return Err("Failed to persist room state.");
        }

        *room = updated_room;

        Ok(dispatch)
    }

    async fn apply_select_media(
        &self,
        media_id: Uuid,
        actor: &str,
    ) -> Result<SocketDispatch, &'static str> {
        let media_item = match self.library.media_item(media_id).await {
            Ok(Some(item)) => item,
            Ok(None) => return Err("Selected media is not in the library."),
            Err(error) => {
                warn!(%error, %media_id, "failed to load media for select");
                return Err("Failed to load the selected media.");
            }
        };

        let now = OffsetDateTime::now_utc();
        let mut room = self.room.write().await;
        let mut updated_room = room.clone();

        updated_room.media_id = Some(media_item.id);
        updated_room.media_title = Some(media_item.file_name.clone());
        updated_room.clock = AuthoritativePlaybackClock::new_paused(now);

        if let Err(error) = self.persistence.save_room(&updated_room).await {
            warn!(%error, room_id = %updated_room.id, "failed to persist media selection");
            return Err("Failed to persist room state.");
        }

        *room = updated_room.clone();
        drop(room);

        Ok(SocketDispatch::Broadcast(ServerEvent::MediaChanged {
            room: updated_room.snapshot(now),
            actor: actor.to_string(),
        }))
    }
}

impl RoomRecord {
    pub(crate) fn new(
        name: String,
        media_id: Option<Uuid>,
        media_title: Option<String>,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            media_id,
            media_title,
            created_at: format_timestamp(now),
            clock: AuthoritativePlaybackClock::new_paused(now),
        }
    }

    pub(crate) fn snapshot(&self, emitted_at: OffsetDateTime) -> Room {
        Room {
            id: self.id,
            name: self.name.clone(),
            media_id: self.media_id,
            media_title: self.media_title.clone(),
            playback_state: self.clock.snapshot(emitted_at),
            created_at: self.created_at.clone(),
        }
    }
}

pub(crate) async fn handle_room_socket(
    mut socket: WebSocket,
    room_id: Uuid,
    room: SharedRoom,
    client_name: String,
) {
    let mut room_events = room.subscribe();
    let connection_count = room.join().await;
    let connection_id = Uuid::new_v4();
    room.add_participant(connection_id, client_name.clone())
        .await;
    let snapshot = room.snapshot().await;
    let participants = room.participant_list().await;

    if send_server_event(
        &mut socket,
        ServerEvent::Snapshot {
            room: snapshot,
            connection_count,
            participants: participants.clone(),
        },
    )
    .await
    .is_err()
    {
        room.remove_participant(connection_id).await;
        room.leave().await;
        return;
    }

    room.broadcast(ServerEvent::PresenceChanged {
        room_id,
        connection_count,
        actor: client_name.clone(),
        joined: true,
    });
    room.broadcast(ServerEvent::ParticipantsUpdated {
        room_id,
        participants,
    });

    loop {
        tokio::select! {
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientSocketMessage>(text.as_str()) {
                            Ok(command) => {
                                match room.apply_socket_message(command, &client_name, connection_id).await {
                                    Ok(SocketDispatch::Broadcast(event)) => room.broadcast(event),
                                    Ok(SocketDispatch::Direct(event)) => {
                                        if send_server_event(&mut socket, event).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(message) => {
                                        if send_server_event(&mut socket, ServerEvent::Error {
                                            message: message.into(),
                                        }).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                warn!(%error, %room_id, "received invalid websocket message");

                                if send_server_event(&mut socket, ServerEvent::Error {
                                    message: "Could not parse the playback command.".into(),
                                }).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        warn!(%error, %room_id, "websocket receive error");
                        break;
                    }
                    None => break,
                }
            }
            event = room_events.recv() => {
                match event {
                    Ok(event) => {
                        if send_server_event(&mut socket, event).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(%skipped, %room_id, "socket lagged behind room events");

                        if send_server_event(&mut socket, ServerEvent::Error {
                            message: format!("Missed {skipped} room updates. Sending a fresh snapshot."),
                        }).await.is_err() {
                            break;
                        }

                        if send_server_event(
                            &mut socket,
                            ServerEvent::Snapshot {
                                room: room.snapshot().await,
                                connection_count: room.connection_count(),
                                participants: room.participant_list().await,
                            },
                        ).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    room.remove_participant(connection_id).await;
    let connection_count = room.leave().await;

    room.broadcast(ServerEvent::PresenceChanged {
        room_id,
        connection_count,
        actor: client_name,
        joined: false,
    });
    room.broadcast(ServerEvent::ParticipantsUpdated {
        room_id,
        participants: room.participant_list().await,
    });
}

async fn send_server_event(socket: &mut WebSocket, event: ServerEvent) -> Result<(), ()> {
    let payload = serde_json::to_string(&event).map_err(|error| {
        warn!(%error, "failed to serialize websocket message");
    })?;

    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(|error| {
            warn!(%error, "failed to send websocket message");
        })
}

/// Ceiling on client-reported one-way latency, in milliseconds. Clients sending
/// larger values would pin the clock anchor arbitrarily far in the past and let
/// peer projections run ahead of reality.
pub(crate) const MAX_CLIENT_ONE_WAY_MS: u32 = 2_000;

pub(crate) fn back_date_for_client_latency(
    now: OffsetDateTime,
    client_one_way_ms: Option<u32>,
) -> OffsetDateTime {
    let clamped = client_one_way_ms.unwrap_or(0).min(MAX_CLIENT_ONE_WAY_MS);
    now - TimeDuration::milliseconds(clamped as i64)
}

pub(crate) fn normalize_command_position(position_seconds: f64) -> Result<f64, &'static str> {
    validate_position(position_seconds).map(|value| round_to(value, 1))
}

pub(crate) fn normalize_reported_position(position_seconds: f64) -> Result<f64, &'static str> {
    validate_position(position_seconds).map(|value| round_to(value, 3))
}

fn validate_position(position_seconds: f64) -> Result<f64, &'static str> {
    if !position_seconds.is_finite() {
        return Err("Playback position must be a valid number.");
    }

    if position_seconds < 0.0 {
        return Err("Playback position cannot be negative.");
    }

    Ok(position_seconds)
}
