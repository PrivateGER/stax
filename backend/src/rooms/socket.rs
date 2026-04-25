use axum::extract::ws::{Message, WebSocket};
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

use crate::protocol::{ClientSocketMessage, ServerEvent};

use super::{SharedRoom, SocketDispatch};

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
