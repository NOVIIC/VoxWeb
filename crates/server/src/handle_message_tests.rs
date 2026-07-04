use super::*;
use voxweb_core::chunk::Position;
use voxweb_core::protocol::{AckReason, ClientMessage, ServerMessage};

/// 在 outbox 中找到第一条匹配 predicate 的消息，返回其 (Recipient, ServerMessage) 副本。
fn find_outbox<F>(server: &Server, pred: F) -> Option<(Recipient, ServerMessage)>
where
    F: Fn(&OutboundMessage) -> bool,
{
    server
        .outbox
        .iter()
        .find(|m| pred(m))
        .map(|m| (m.recipient.clone(), m.message.clone()))
}

#[test]
fn add_player_allocates_increasing_ids_and_enqueues_welcome_peerjoined_snapshot() {
    let mut server = Server::new(42);
    let eid1 = server.add_player("Alice".into());
    let eid2 = server.add_player("Bob".into());
    assert_eq!(eid1, 1);
    assert_eq!(eid2, 2);
    assert!(server.players.contains_key(&eid1));
    assert!(server.players.contains_key(&eid2));

    // Welcome 应给 eid1 和 eid2 各一条
    let welcome1 = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::Welcome { entity_id, .. }) if *e == 1 && *entity_id == 1
        )
    });
    let welcome2 = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::Welcome { entity_id, .. }) if *e == 2 && *entity_id == 2
        )
    });
    assert!(welcome1.is_some(), "missing Welcome to eid 1");
    assert!(welcome2.is_some(), "missing Welcome to eid 2");

    // PeerJoined：第一次 add_player 也会 enqueue（即使没有别人；Except 让 outbox 路由层自然过滤）
    let pj_for_bob = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::Except(e), ServerMessage::PeerJoined { entity_id, .. }) if *e == 2 && *entity_id == 2
        )
    });
    assert!(pj_for_bob.is_some(), "missing PeerJoined for eid 2");

    // FieldSnapshot：至少有一片到 eid1
    let has_snapshot = server.outbox.iter().any(|m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::FieldSnapshot { .. }) if *e == 1
        )
    });
    assert!(has_snapshot, "expected at least one FieldSnapshot fragment");
}

#[test]
fn remove_player_enqueues_peer_left_to_all() {
    let mut server = Server::new(0);
    let eid = server.add_player("Alice".into());
    server.drain_outbox(); // 清掉 add_player 产生的消息

    server.remove_player(eid);

    let pl = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::All, ServerMessage::PeerLeft { entity_id }) if *entity_id == eid
        )
    });
    assert!(pl.is_some(), "missing PeerLeft");
    assert!(!server.players.contains_key(&eid));
}

#[test]
fn remove_unknown_player_is_no_op() {
    let mut server = Server::new(0);
    server.remove_player(999);
    assert!(server.outbox.is_empty());
}

#[test]
fn drain_outbox_empties_queue() {
    let mut server = Server::new(0);
    server.add_player("A".into());
    let before = server.outbox.len();
    assert!(before > 0);
    let drained = server.drain_outbox();
    assert_eq!(drained.len(), before);
    assert!(server.outbox.is_empty());
}

#[test]
fn handle_message_player_input_updates_player_entity_and_rejects_old_tick() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    server.handle_message(
        eid,
        ClientMessage::PlayerInput {
            tick: 5,
            position: Vec3::new(1.0, 70.0, 2.0),
            yaw: 0.1,
            pitch: -0.2,
        },
    );
    let p = &server.players[&eid];
    assert_eq!(p.position, Vec3::new(1.0, 70.0, 2.0));
    assert_eq!(p.last_input_tick, 5);

    server.handle_message(
        eid,
        ClientMessage::PlayerInput {
            tick: 3,
            position: Vec3::new(100.0, 70.0, 2.0),
            yaw: 0.0,
            pitch: 0.0,
        },
    );
    let p = &server.players[&eid];
    assert_eq!(
        p.position,
        Vec3::new(1.0, 70.0, 2.0),
        "expired tick must not override"
    );
    assert_eq!(p.last_input_tick, 5);
}

#[test]
fn handle_message_field_request_generates_and_sends_snapshot() {
    let mut server = Server::new(0);
    server.set_host_render_distance(8);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    let requested = ChunkPos::new(7, 0);
    assert!(!server.world.chunks.contains_key(&requested));
    server.handle_message(
        eid,
        ClientMessage::FieldRequest {
            center: ChunkPos::new(0, 0),
            render_distance: 8,
            chunks: vec![requested],
        },
    );

    assert!(server.world.chunks.contains_key(&requested));
    let snapshot = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::FieldSnapshot { pos, .. })
                if *e == eid && *pos == requested
        )
    });
    assert!(snapshot.is_some(), "missing requested FieldSnapshot");
}

#[test]
fn handle_message_field_request_ignores_far_chunks() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    let far = ChunkPos::new(40, 0);
    server.handle_message(
        eid,
        ClientMessage::FieldRequest {
            center: ChunkPos::new(0, 0),
            render_distance: 10,
            chunks: vec![far],
        },
    );

    let snapshot = find_outbox(
        &server,
        |m| matches!(m.message, ServerMessage::FieldSnapshot { pos, .. } if pos == far),
    );
    assert!(snapshot.is_none(), "far chunk request should be ignored");
}

#[test]
fn handle_message_field_request_is_capped_by_host_render_distance() {
    let mut server = Server::new(0);
    server.set_host_render_distance(4);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    let allowed = ChunkPos::new(4, 0);
    let denied = ChunkPos::new(5, 0);
    server.handle_message(
        eid,
        ClientMessage::FieldRequest {
            center: ChunkPos::new(0, 0),
            render_distance: 10,
            chunks: vec![allowed, denied],
        },
    );

    assert!(server.world.chunks.contains_key(&allowed));
    assert!(!server.world.chunks.contains_key(&denied));
}

/// 把 chunk(0,0) 的一柱方块设置好，便于挖放测试。
fn prepare_world() -> (Server, EntityId) {
    let mut server = Server::new(0);
    let eid = server.add_player("Tester".into());
    server.drain_outbox(); // 清掉初始消息

    server
        .world
        .ensure_chunk_generated(voxweb_core::ChunkPos::new(0, 0));
    for x in 0..16 {
        for z in 0..16 {
            server
                .world
                .set_block(Position::new(x, 64, z), voxweb_core::BlockID::STONE);
            server
                .world
                .set_block(Position::new(x, 65, z), voxweb_core::BlockID::AIR);
        }
    }
    server.players.get_mut(&eid).unwrap().position = Vec3::new(3.5, 65.0, 3.5);
    (server, eid)
}

#[test]
fn handle_message_break_enqueues_ack_one_and_field_delta_all() {
    let (mut server, eid) = prepare_world();
    server.handle_message(
        eid,
        ClientMessage::Break {
            pos: Position::new(3, 64, 3),
            request_id: 42,
            input_tick: 5,
            player_position: Vec3::new(3.5, 65.0, 3.5),
        },
    );

    let ack = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::ActionAck { request_id, accepted: true, .. })
                if *e == eid && *request_id == 42
        )
    });
    assert!(ack.is_some(), "missing ActionAck One({eid})");

    let bu = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::All, ServerMessage::FieldDelta { cell, .. })
                if cell.to_block_id() == voxweb_core::BlockID::AIR
        )
    });
    assert!(bu.is_some(), "missing FieldDelta All");

    assert_eq!(
        server.world.get_block(Position::new(3, 64, 3)),
        voxweb_core::BlockID::AIR
    );
}

#[test]
fn handle_message_break_broadcasts_free_object_spawn_for_floating_hard_block() {
    let (mut server, eid) = prepare_world();
    let support = Position::new(3, 64, 3);
    let lower_support = Position::new(3, 63, 3);
    let floating = Position::new(3, 65, 3);
    server
        .world
        .set_block(lower_support, voxweb_core::BlockID::STONE);
    server
        .world
        .set_block(floating, voxweb_core::BlockID::STONE);

    server.handle_message(
        eid,
        ClientMessage::Break {
            pos: support,
            request_id: 43,
            input_tick: 6,
            player_position: Vec3::new(3.5, 65.0, 3.5),
        },
    );

    assert_eq!(server.world.get_block(floating), voxweb_core::BlockID::AIR);
    assert_eq!(server.world.get_block(support), voxweb_core::BlockID::AIR);
    assert!(!server.world.free_objects.is_empty());

    let spawn = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (
                Recipient::All,
                ServerMessage::FreeObjectSpawn {
                    object_id: _,
                    cells,
                }
            ) if cells.iter().any(|(pos, cell)| {
                *pos == floating && cell.to_block_id() == voxweb_core::BlockID::STONE
            })
        )
    });
    assert!(
        spawn.is_some(),
        "missing FreeObjectSpawn with extracted cells"
    );

    for _ in 0..60 {
        server.tick();
        if server.outbox.iter().any(|m| {
            matches!(
                (&m.recipient, &m.message),
                (
                    Recipient::All,
                    ServerMessage::FreeObjectProjectBatch { projections }
                ) if projections.iter().any(|(_, deltas)| {
                    deltas.iter().any(|(_, cell)| cell.to_block_id() == voxweb_core::BlockID::STONE)
                })
            )
        }) {
            break;
        }
    }
    let projection = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (
                Recipient::All,
                ServerMessage::FreeObjectProjectBatch { projections }
            ) if projections.iter().any(|(_, deltas)| {
                deltas.iter().any(|(_, cell)| cell.to_block_id() == voxweb_core::BlockID::STONE)
            })
        )
    });
    assert!(
        projection.is_some(),
        "missing FreeObjectProjectBatch after dynamic object settles"
    );
}

#[test]
fn handle_message_break_out_of_range_only_ack_no_field_delta() {
    let (mut server, eid) = prepare_world();
    server.handle_message(
        eid,
        ClientMessage::Break {
            pos: Position::new(15, 64, 15),
            request_id: 7,
            input_tick: 5,
            player_position: Vec3::new(3.5, 65.0, 3.5),
        },
    );
    let ack = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (
                Recipient::One(_),
                ServerMessage::ActionAck {
                    accepted: false,
                    reason: AckReason::OutOfRange,
                    request_id: 7
                }
            )
        )
    });
    assert!(ack.is_some());
    let bu = find_outbox(&server, |m| {
        matches!(m.message, ServerMessage::FieldDelta { .. })
    });
    assert!(
        bu.is_none(),
        "out-of-range break should not enqueue FieldDelta"
    );
    assert_eq!(
        server.world.get_block(Position::new(15, 64, 15)),
        voxweb_core::BlockID::STONE
    );
}

#[test]
fn handle_message_place_overlap_rejected_and_no_field_delta() {
    let (mut server, eid) = prepare_world();
    server.handle_message(
        eid,
        ClientMessage::Place {
            pos: Position::new(3, 65, 3),
            block: voxweb_core::BlockID::STONE,
            request_id: 9,
            input_tick: 5,
            player_position: Vec3::new(3.5, 65.0, 3.5),
        },
    );
    let ack = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (
                Recipient::One(_),
                ServerMessage::ActionAck {
                    accepted: false,
                    reason: AckReason::Overlap,
                    request_id: 9
                }
            )
        )
    });
    assert!(ack.is_some());
    let bu = find_outbox(&server, |m| {
        matches!(m.message, ServerMessage::FieldDelta { .. })
    });
    assert!(bu.is_none());
}

#[test]
fn handle_message_place_granular_spawns_falling_grain() {
    let (mut server, eid) = prepare_world();
    let placed = Position::new(5, 66, 3);
    let settled = Position::new(5, 65, 3);
    server.world.set_block(placed, voxweb_core::BlockID::AIR);
    server.world.set_block(settled, voxweb_core::BlockID::AIR);

    server.handle_message(
        eid,
        ClientMessage::Place {
            pos: placed,
            block: voxweb_core::BlockID::SAND,
            request_id: 99,
            input_tick: 6,
            player_position: Vec3::new(3.5, 65.0, 3.5),
        },
    );

    // 放置即时只落在放置点并广播编辑 FieldDelta；下落改由后续 tick 逐格模拟。
    assert_eq!(
        server.world.get_block(placed),
        voxweb_core::BlockID::SAND,
        "放置后沙子先停在放置点，尚未下落"
    );
    assert!(server.outbox.iter().any(|m| {
        matches!(
            m.message,
            ServerMessage::FieldDelta { pos, cell }
                if pos == placed && cell.to_block_id() == voxweb_core::BlockID::SAND
        )
    }));

    // 驱动 tick：颗粒提取（SpawnBatch）→ 自由落体 → 落定（ProjectBatch）。
    let mut spawned = false;
    let mut projected = false;
    for _ in 0..180 {
        server.tick();
        if server.outbox.iter().any(|m| {
            matches!(
                &m.message,
                ServerMessage::FreeObjectSpawnBatch { spawns }
                    if spawns.iter().any(|(_, cells)| cells.iter().any(|(pos, cell)| {
                        *pos == placed && cell.to_block_id() == voxweb_core::BlockID::SAND
                    }))
            )
        }) {
            spawned = true;
        }
        if server.outbox.iter().any(|m| {
            matches!(
                &m.message,
                ServerMessage::FreeObjectProjectBatch { projections }
                    if projections.iter().any(|(_, deltas)| deltas.iter().any(|(pos, cell)| {
                        *pos == settled && cell.to_block_id() == voxweb_core::BlockID::SAND
                    }))
            )
        }) {
            projected = true;
        }
        if server.world.get_block(settled) == voxweb_core::BlockID::SAND {
            break;
        }
    }

    assert!(spawned, "应广播 FreeObjectSpawnBatch 提取下落颗粒");
    assert!(projected, "应广播 FreeObjectProjectBatch 记录落定");
    assert_eq!(server.world.get_block(placed), voxweb_core::BlockID::AIR);
    assert_eq!(server.world.get_block(settled), voxweb_core::BlockID::SAND);
}

#[test]
fn handle_message_break_uses_action_position_when_player_input_lags() {
    let (mut server, eid) = prepare_world();
    server.players.get_mut(&eid).unwrap().position = Vec3::new(30.0, 65.0, 30.0);

    server.handle_message(
        eid,
        ClientMessage::Break {
            pos: Position::new(3, 64, 3),
            request_id: 77,
            input_tick: 12,
            player_position: Vec3::new(3.5, 65.0, 3.5),
        },
    );

    assert_eq!(
        server.world.get_block(Position::new(3, 64, 3)),
        voxweb_core::BlockID::AIR
    );
    let player = server.players.get(&eid).unwrap();
    assert_eq!(player.last_input_tick, 12);
    assert!((player.position - Vec3::new(3.5, 65.0, 3.5)).length() < 0.001);
}

#[test]
fn handle_message_ping_returns_pong_one_with_server_clock() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.set_clock(12345);
    server.drain_outbox();

    server.handle_message(eid, ClientMessage::Ping { client_time_ms: 7 });
    let pong = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::Pong { client_time_ms: 7, server_time_ms: 12345 })
                if *e == eid
        )
    });
    assert!(pong.is_some());
}

#[test]
fn handle_message_chat_broadcasts_to_all() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    server.handle_message(
        eid,
        ClientMessage::Chat {
            content: "hi".into(),
        },
    );
    let chat = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::All, ServerMessage::Chat { from, content })
                if *from == eid && content == "hi"
        )
    });
    assert!(chat.is_some());
}

#[test]
fn host_eid_set_on_first_add_player() {
    let mut server = Server::new(0);
    assert!(server.host_entity_id().is_none());

    let host = server.add_player("Alice".into());
    assert_eq!(server.host_entity_id(), Some(host));

    let _ = server.add_player("Bob".into());
    assert_eq!(server.host_entity_id(), Some(host));
}

#[test]
fn set_host_render_distance_broadcasts_after_host_exists() {
    let mut server = Server::new(0);
    let _ = server.add_player("Alice".into());
    server.drain_outbox();

    server.set_host_render_distance(4);

    let settings = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::All, ServerMessage::HostSettings { render_distance }) if *render_distance == 4
        )
    });
    assert!(settings.is_some(), "missing HostSettings broadcast");
}

#[test]
fn welcome_carries_full_roster_and_host_eid() {
    let mut server = Server::new(0);
    let alice = server.add_player("Alice".into());
    server.drain_outbox();

    let bob = server.add_player("Bob".into());
    let welcome = find_outbox(&server, |m| {
        matches!(
            (&m.recipient, &m.message),
            (Recipient::One(e), ServerMessage::Welcome { entity_id, .. })
                if *e == bob && *entity_id == bob
        )
    });
    let (_, msg) = welcome.expect("missing Welcome to bob");
    match msg {
        ServerMessage::Welcome {
            host_entity_id,
            host_render_distance,
            players,
            ..
        } => {
            assert_eq!(host_entity_id, alice, "host_eid should be Alice");
            assert_eq!(host_render_distance, DEFAULT_HOST_RENDER_DISTANCE);
            let mut names: Vec<_> = players.iter().map(|p| p.display_name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, vec!["Alice", "Bob"]);
            let ids: Vec<EntityId> = {
                let mut v: Vec<_> = players.iter().map(|p| p.entity_id).collect();
                v.sort_unstable();
                v
            };
            assert_eq!(ids, vec![alice, bob]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn chat_drops_messages_over_256_chars() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    let too_long: String = std::iter::repeat_n('x', 257).collect();
    server.handle_message(eid, ClientMessage::Chat { content: too_long });
    let chat = find_outbox(&server, |m| matches!(m.message, ServerMessage::Chat { .. }));
    assert!(
        chat.is_none(),
        "expected too-long chat to be silently dropped"
    );

    let ok_long: String = std::iter::repeat_n('y', 256).collect();
    server.handle_message(
        eid,
        ClientMessage::Chat {
            content: ok_long.clone(),
        },
    );
    let chat = find_outbox(
        &server,
        |m| matches!(&m.message, ServerMessage::Chat { content, .. } if content == &ok_long),
    );
    assert!(chat.is_some(), "256-char chat should pass");
}

#[test]
fn chat_drop_counts_unicode_scalars_not_bytes() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    let cn: String = std::iter::repeat_n('你', 256).collect();
    server.handle_message(eid, ClientMessage::Chat { content: cn });
    let chat = find_outbox(&server, |m| matches!(m.message, ServerMessage::Chat { .. }));
    assert!(chat.is_some(), "256-char unicode should pass");
}

#[test]
fn chat_rate_limit_drops_after_5_per_3s() {
    let mut server = Server::new(0);
    let eid = server.add_player("A".into());
    server.drain_outbox();

    for i in 0..6 {
        server.handle_message(
            eid,
            ClientMessage::Chat {
                content: format!("m{i}"),
            },
        );
    }
    let count = server
        .outbox
        .iter()
        .filter(|m| matches!(m.message, ServerMessage::Chat { .. }))
        .count();
    assert_eq!(count, 5, "expected 5 chats to pass, got {count}");

    server.drain_outbox();
    server.tick = server.tick.saturating_add(CHAT_RATE_WINDOW_TICKS + 1);
    server.handle_message(
        eid,
        ClientMessage::Chat {
            content: "after-window".into(),
        },
    );
    let count_after = server
        .outbox
        .iter()
        .filter(|m| matches!(&m.message, ServerMessage::Chat { content, .. } if content == "after-window"))
        .count();
    assert_eq!(count_after, 1, "expected chat to pass after window expiry");
}

#[test]
fn tick_enqueues_player_tick_with_all_players_to_all() {
    let mut server = Server::new(0);
    let eid1 = server.add_player("A".into());
    let eid2 = server.add_player("B".into());
    server.drain_outbox();

    server.set_clock(5000);
    server.tick();

    let pt = find_outbox(&server, |m| {
        matches!(m.message, ServerMessage::PlayerTick { .. })
    });
    assert!(pt.is_some());
    if let Some((
        rec,
        ServerMessage::PlayerTick {
            players,
            server_time_ms,
            ..
        },
    )) = pt
    {
        assert_eq!(rec, Recipient::All);
        assert_eq!(server_time_ms, 5000);
        assert_eq!(players.len(), 2);
        assert!(players.iter().any(|p| p.entity_id == eid1));
        assert!(players.iter().any(|p| p.entity_id == eid2));
    }
}

#[test]
fn tick_without_players_does_not_enqueue_player_tick() {
    let mut server = Server::new(0);
    server.tick();
    let pt = find_outbox(&server, |m| {
        matches!(m.message, ServerMessage::PlayerTick { .. })
    });
    assert!(pt.is_none(), "empty world should not produce PlayerTick");
}

#[test]
fn hello_in_handle_message_is_warning_no_op() {
    let mut server = Server::new(0);
    server.handle_message(
        999,
        ClientMessage::Hello {
            display_name: "X".into(),
            version: 1,
        },
    );
    assert!(server.players.is_empty());
    assert!(server.outbox.is_empty());
}
