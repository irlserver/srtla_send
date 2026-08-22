#[cfg(test)]
mod tests {

    use srtla_core::connection::STARTUP_GRACE_MS;
    use srtla_core::registration::*;
    use srtla_core::utils::now_ms;
    use srtla_protocol::*;

    use crate::test_helpers::create_test_connection;

    #[test]
    fn test_registration_manager_creation() {
        let reg = SrtlaRegistrationManager::new();

        assert_eq!(reg.active_connections(), 0);
        assert!(!reg.has_connected());
        assert!(!reg.broadcast_reg2_pending());
        assert_eq!(reg.pending_reg2_idx(), None);
        assert_eq!(reg.reg1_target_idx(), None);

        // ID should be randomly generated and non-zero
        assert!(!reg.srtla_id().iter().all(|&b| b == 0));
    }

    #[test]
    fn test_reg_ngp_handling() {
        let mut reg = SrtlaRegistrationManager::new();

        // Create REG_NGP packet
        let mut buf = vec![0u8; 4];
        buf[0..2].copy_from_slice(&SRTLA_TYPE_REG_NGP.to_be_bytes());

        // Process REG_NGP from connection 1
        let handled = reg.process_registration_packet(1, &buf, now_ms());
        assert!(handled.is_some());
        assert_eq!(reg.reg1_target_idx(), Some(1));

        let current_time = now_ms();
        assert!(reg.reg1_next_send_at_ms() <= current_time + 100); // Allow CI scheduler skew
    }

    #[test]
    fn test_reg2_handling() {
        let mut reg = SrtlaRegistrationManager::new();

        // Set up pending REG2 state
        reg.set_pending_reg2_idx(Some(0));
        let original_id = reg.srtla_id;

        // Create REG2 response packet with modified ID
        let mut modified_id = original_id;
        modified_id[SRTLA_ID_LEN / 2..].fill(0xab); // Server modifies last half
        let buf = create_reg2_packet(&modified_id);

        let handled = reg.process_registration_packet(0, &buf, now_ms());
        assert!(handled.is_some());

        // Should have updated the ID and set broadcast pending
        assert_eq!(reg.srtla_id, modified_id);
        assert_eq!(reg.pending_reg2_idx(), None);
        assert!(reg.broadcast_reg2_pending());
        assert_eq!(reg.reg1_target_idx(), None);
    }

    #[test]
    fn test_reg3_handling() {
        let mut reg = SrtlaRegistrationManager::new();

        assert_eq!(reg.active_connections(), 0);
        assert!(!reg.has_connected);

        // Create REG3 packet
        let buf = vec![(SRTLA_TYPE_REG3 >> 8) as u8, (SRTLA_TYPE_REG3 & 0xff) as u8];

        // A REG3 is only honored on an uplink we actually queued a REG2 for.
        let _ = reg.build_reg2(2);
        let handled = reg.process_registration_packet(2, &buf, now_ms());
        assert!(handled.is_some());

        // REG3 should set has_connected flag
        // Note: active_connections is updated by update_active_connections() during housekeeping
        assert!(reg.has_connected);
    }

    #[test]
    fn test_reg_err_handling() {
        let mut reg = SrtlaRegistrationManager::new();

        // Set up pending state
        reg.set_pending_reg2_idx(Some(1));
        reg.set_pending_timeout_at_ms(now_ms() + 5000);
        reg.set_reg1_target_idx(Some(1));

        // Create REG_ERR packet
        let mut buf = vec![0u8; 4];
        buf[0..2].copy_from_slice(&SRTLA_TYPE_REG_ERR.to_be_bytes());

        let handled = reg.process_registration_packet(1, &buf, now_ms());
        assert!(handled.is_some());

        // Should clear pending state and wait for a new REG_NGP before retrying
        let after = now_ms();
        assert_eq!(reg.pending_reg2_idx(), None);
        assert_eq!(reg.pending_timeout_at_ms(), 0);
        assert_eq!(reg.reg1_target_idx(), None);
        assert!(reg.reg1_next_send_at_ms() >= after + REG2_TIMEOUT * 1000);
    }

    #[test]
    fn test_unrecognized_packet() {
        let mut reg = SrtlaRegistrationManager::new();

        // Create a non-registration packet
        let mut buf = vec![0u8; 4];
        buf[0..2].copy_from_slice(&SRT_TYPE_ACK.to_be_bytes());

        let handled = reg.process_registration_packet(0, &buf, now_ms());
        assert!(handled.is_none());
    }

    #[tokio::test]
    async fn test_reg_driver_initial_reg1() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = [create_test_connection().await];

        let mut ngp = vec![0u8; 2];
        ngp[0..2].copy_from_slice(&SRTLA_TYPE_REG_NGP.to_be_bytes());
        reg.process_registration_packet(0, &ngp, now_ms());

        // Should send REG1 to first connection when no connections are active
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        assert_eq!(reg.pending_reg2_idx(), Some(0));
        assert!(reg.pending_timeout_at_ms() > now_ms());
    }

    #[tokio::test]
    async fn test_reg_driver_with_target() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = [
            create_test_connection().await,
            create_test_connection().await,
        ];

        // Set a specific target from REG_NGP
        reg.set_reg1_target_idx(Some(1));

        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        // Should send to the specified target
        assert_eq!(reg.pending_reg2_idx(), Some(1));
    }

    #[tokio::test]
    async fn test_reg_driver_waits_for_ngp() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = [create_test_connection().await];

        // Without REG_NGP, nothing should happen
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert_eq!(reg.pending_reg2_idx(), None);

        let mut ngp = vec![0u8; 2];
        ngp[0..2].copy_from_slice(&SRTLA_TYPE_REG_NGP.to_be_bytes());
        reg.process_registration_packet(0, &ngp, now_ms());

        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert_eq!(reg.pending_reg2_idx(), Some(0));
    }

    #[tokio::test]
    async fn test_broadcast_reg2() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = [
            create_test_connection().await,
            create_test_connection().await,
        ];

        // Trigger broadcast
        reg.set_broadcast_reg2_pending(true);
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        // Should have cleared the broadcast flag
        assert!(!reg.broadcast_reg2_pending());
    }

    #[tokio::test]
    async fn test_send_reg1_to_sets_pending_state() {
        let mut reg = SrtlaRegistrationManager::new();
        reg.build_reg1_for(0, now_ms());

        assert_eq!(reg.pending_reg2_idx(), Some(0));
        assert_eq!(reg.reg1_target_idx(), Some(0));
        let now = now_ms();
        assert!(reg.pending_timeout_at_ms() >= now);
        assert!(reg.reg1_next_send_at_ms() >= now);
        assert!(reg.reg1_next_send_at_ms() <= reg.pending_timeout_at_ms());
    }

    #[tokio::test]
    async fn test_send_reg2_to_does_not_override_state() {
        let mut reg = SrtlaRegistrationManager::new();
        // Pretend we already have pending state for another index
        reg.set_pending_reg2_idx(Some(1));
        reg.set_reg1_target_idx(Some(1));

        let _ = reg.build_reg2(0);

        // State should remain unchanged
        assert_eq!(reg.pending_reg2_idx(), Some(1));
        assert_eq!(reg.reg1_target_idx(), Some(1));
    }

    #[test]
    fn test_clear_pending_if_timed_out() {
        let mut reg = SrtlaRegistrationManager::new();

        let start = now_ms();
        reg.set_pending_reg2_idx(Some(0));
        reg.set_pending_timeout_at_ms(start + 10);
        reg.set_reg1_target_idx(Some(0));
        reg.set_reg1_next_send_at_ms(start + 1000);

        // Advance time beyond timeout
        let cleared_time = start + 20;
        let cleared = reg.clear_pending_if_timed_out(cleared_time);

        assert_eq!(cleared, Some(0));
        assert_eq!(reg.pending_reg2_idx(), None);
        assert_eq!(reg.pending_timeout_at_ms(), 0);
        assert_eq!(reg.reg1_target_idx(), None);
        assert_eq!(reg.reg1_next_send_at_ms(), cleared_time);
    }

    #[tokio::test]
    async fn test_multiple_reg3_connections() {
        let mut reg = SrtlaRegistrationManager::new();
        let reg3_packet = vec![0x92, 0x02];

        // Create 3 test connections
        let connections = vec![
            create_test_connection().await,
            create_test_connection().await,
            create_test_connection().await,
        ];

        // The REG2 broadcast (one per uplink) is what authorizes the REG3s.
        reg.set_broadcast_reg2_pending(true);
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        // Simulate multiple REG3 responses
        for i in 0..3 {
            let handled = reg.process_registration_packet(i, &reg3_packet, now_ms());
            assert!(handled.is_some());
        }

        assert!(reg.has_connected);

        // Update active connections count based on connection states
        reg.update_active_connections(&connections);

        // All 3 connections should be active (none timed out)
        assert_eq!(reg.active_connections(), 3);
    }

    #[tokio::test]
    async fn test_reg_driver_timing() {
        let mut reg = SrtlaRegistrationManager::new();

        // Set future send time to prevent immediate sending
        reg.set_reg1_next_send_at_ms(now_ms() + 5000);

        // Even with connections available, should not send yet
        let connections = [create_test_connection().await];
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert_eq!(
            reg.pending_reg2_idx(),
            None,
            "Should not send before next-send time"
        );
    }

    #[tokio::test]
    async fn test_registration_state_transitions() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = [create_test_connection().await];

        // Initial state
        assert_eq!(reg.active_connections(), 0);
        assert!(!reg.has_connected);

        // After REG_NGP
        let ngp_packet = [0x92, 0x11, 0x00, 0x00];
        reg.process_registration_packet(0, &ngp_packet, now_ms());
        assert_eq!(reg.reg1_target_idx(), Some(0));

        // Set up for REG2
        reg.set_pending_reg2_idx(Some(0));

        // Process REG2
        let mut modified_id = reg.srtla_id;
        modified_id[SRTLA_ID_LEN / 2..].fill(0xff);
        let reg2_packet = create_reg2_packet(&modified_id);
        reg.process_registration_packet(0, &reg2_packet, now_ms());

        assert!(reg.broadcast_reg2_pending());
        assert_eq!(reg.pending_reg2_idx(), None);

        // Drive the broadcast so the uplink holds a REG3 grant, as housekeeping does
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        // Process REG3
        let reg3_packet = vec![0x92, 0x02];
        reg.process_registration_packet(0, &reg3_packet, now_ms());

        assert!(reg.has_connected);

        // Update active connections count based on connection states
        reg.update_active_connections(&connections);
        assert_eq!(reg.active_connections(), 1);
    }

    #[test]
    fn test_id_generation_uniqueness() {
        let ids: Vec<_> = (0..8)
            .map(|_| SrtlaRegistrationManager::new().srtla_id)
            .collect();
        let all_same = ids.windows(2).all(|w| w[0] == w[1]);
        assert!(!all_same, "All generated IDs were identical unexpectedly");
    }

    #[tokio::test]
    async fn test_start_probing_emits_a_probe_per_connection() {
        // Sends now lift to the shell, so `start_probing` can no longer fail to
        // send: it optimistically records one probe per connection and enters
        // `WaitingForProbes`. The fallback-when-no-response path is exercised by
        // `test_check_probing_no_responses_uses_fallback` via the probe timeout.
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![
            create_test_connection().await,
            create_test_connection().await,
        ];

        assert_eq!(reg.reg1_target_idx(), None);

        let probes = reg.start_probing(&mut connections, now_ms());

        assert_eq!(probes.len(), 2, "one probe packet queued per connection");
        assert!(reg.is_probing(), "driver is now awaiting probe responses");
    }

    #[tokio::test]
    async fn test_probing_skipped_when_active_connections() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![create_test_connection().await];

        connections[0].connected = true;
        connections[0].last_received = Some(now_ms());
        reg.update_active_connections(&connections);

        let initial_target = reg.reg1_target_idx();
        let _ = reg.start_probing(&mut connections, now_ms());

        assert_eq!(reg.reg1_target_idx(), initial_target);
        assert!(!reg.is_probing());
    }

    #[test]
    fn test_probe_response_tracking() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();
        reg.simulate_probe_result(0, 100);
        reg.simulate_probe_result(1, 200);

        assert_eq!(reg.probe_results_count(), 2);
    }

    #[test]
    fn test_check_probing_complete_selects_lowest_rtt() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();
        reg.simulate_probe_result(0, 150);
        reg.simulate_probe_result(1, 50);
        reg.simulate_probe_result(2, 200);

        let completed = reg.check_probing_complete();

        assert!(completed);
        assert_eq!(reg.reg1_target_idx(), Some(1));
        assert!(!reg.is_probing());
    }

    #[test]
    fn test_check_probing_timeout_selects_best_available() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();
        reg.simulate_probe_result(0, 100);

        std::thread::sleep(std::time::Duration::from_millis(2100));

        let completed = reg.check_probing_complete();

        assert!(completed);
        assert_eq!(reg.reg1_target_idx(), Some(0));
        assert!(!reg.is_probing());
    }

    #[test]
    fn test_check_probing_no_responses_uses_fallback() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();

        std::thread::sleep(std::time::Duration::from_millis(2100));

        let completed = reg.check_probing_complete();

        assert!(completed);
        assert_eq!(reg.reg1_target_idx(), Some(0));
        assert!(!reg.is_probing());
    }

    #[test]
    fn test_handle_probe_response_records_rtt() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();
        reg.simulate_probe_result(0, 0);
        reg.simulate_probe_result(1, 0);

        std::thread::sleep(std::time::Duration::from_millis(50));
        reg.handle_probe_response(0, now_ms());

        std::thread::sleep(std::time::Duration::from_millis(50));
        reg.handle_probe_response(1, now_ms());

        let completed = reg.check_probing_complete();

        assert!(completed);
        assert_eq!(reg.reg1_target_idx(), Some(0));
    }

    #[test]
    fn test_reg_ngp_during_probing_handled_as_probe_response() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();
        reg.simulate_probe_result(0, 0);

        let ngp_packet = [0x92, 0x11, 0x00, 0x00];
        std::thread::sleep(std::time::Duration::from_millis(50));

        reg.process_registration_packet(0, &ngp_packet, now_ms());

        assert!(reg.is_probing());
        assert_eq!(reg.probe_results_count(), 1);
    }

    #[test]
    fn test_reg_ngp_after_probing_updates_target() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_probing_state_waiting();
        reg.simulate_probe_result(0, 100);
        reg.check_probing_complete();

        assert!(!reg.is_probing());
        assert_eq!(reg.reg1_target_idx(), Some(0));

        let ngp_packet = [0x92, 0x11, 0x00, 0x00];
        reg.process_registration_packet(1, &ngp_packet, now_ms());

        assert_eq!(reg.reg1_target_idx(), Some(1));
    }

    // Two-phase SRTLA v2 handshake, driven from the sender side:
    // REG1 -> REG2(full_id) -> REG2 broadcast -> REG3.
    #[tokio::test]
    async fn reg_handshake_two_phase_flow() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = [
            create_test_connection().await,
            create_test_connection().await,
        ];

        let mut ngp = vec![0u8; 2];
        ngp[0..2].copy_from_slice(&SRTLA_TYPE_REG_NGP.to_be_bytes());
        reg.process_registration_packet(0, &ngp, now_ms());
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert_eq!(
            reg.pending_reg2_idx(),
            Some(0),
            "REG1 sent on conn 0 -> awaiting REG2"
        );

        let sender_prefix = reg.srtla_id;
        let mut full_id = sender_prefix;
        full_id[SRTLA_ID_LEN / 2..].fill(0x5a);
        reg.process_registration_packet(0, &create_reg2_packet(&full_id), now_ms());

        assert_eq!(reg.srtla_id, full_id, "conn 0 adopts the receiver full_id");
        assert!(reg.broadcast_reg2_pending(), "REG2 broadcast queued");
        assert_eq!(reg.pending_reg2_idx(), None, "REG2 received clears pending");

        let broadcast = create_reg2_packet(&reg.srtla_id);
        assert_eq!(
            &broadcast[2..],
            &full_id[..],
            "broadcast REG2 carries the full_id to conn N (not just conn 0)"
        );
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert!(
            !reg.broadcast_reg2_pending(),
            "REG2 broadcast consumed after sending to all uplinks"
        );

        let reg3 = vec![(SRTLA_TYPE_REG3 >> 8) as u8, (SRTLA_TYPE_REG3 & 0xff) as u8];
        for idx in 0..connections.len() {
            assert!(
                reg.process_registration_packet(idx, &reg3, now_ms())
                    .is_some(),
                "REG3 on conn {idx} handled"
            );
        }
        assert!(reg.has_connected(), "REG3 marks the handshake complete");
    }

    // REG2 reply echoes the client id in full_id: the first SRTLA_ID_LEN/2 bytes
    // echo the sender id, the tail is receiver-substituted.
    #[test]
    fn full_id_propagation_byte_wise() {
        let mut reg = SrtlaRegistrationManager::new();
        reg.set_pending_reg2_idx(Some(0));

        let sender_id = reg.srtla_id;
        let half = SRTLA_ID_LEN / 2;

        let mut full_id = sender_id;
        for b in full_id[half..].iter_mut() {
            *b = 0xc3;
        }
        reg.process_registration_packet(0, &create_reg2_packet(&full_id), now_ms());

        assert_eq!(
            &reg.srtla_id[..half],
            &sender_id[..half],
            "first half (sender prefix) must be preserved byte-for-byte"
        );
        assert_eq!(
            &reg.srtla_id[half..],
            &full_id[half..],
            "second half must equal the receiver-substituted tail"
        );
        for (i, &b) in reg.srtla_id[half..].iter().enumerate() {
            assert_eq!(b, 0xc3, "tail byte {i} not substituted");
        }
    }

    // Registration timing is wall-clock (now_ms == SystemTime), so the timeout is
    // exercised through the production seam clear_pending_if_timed_out with explicit
    // logical now values — never a real sleep; the paused clock keeps it deterministic.
    #[tokio::test(start_paused = true)]
    async fn reg2_timeout_fires_at_4s_logical() {
        let mut reg = SrtlaRegistrationManager::new();
        let base = now_ms();
        reg.build_reg1_for(0, base);
        assert_eq!(reg.pending_reg2_idx(), Some(0));

        let deadline = reg.pending_timeout_at_ms();
        assert!(
            deadline >= base + REG2_TIMEOUT * 1000 && deadline <= now_ms() + REG2_TIMEOUT * 1000,
            "REG2 deadline must be REG2_TIMEOUT (4s) past the REG1 send"
        );

        assert_eq!(
            reg.clear_pending_if_timed_out(deadline - 1),
            None,
            "must not time out before REG2_TIMEOUT"
        );
        assert_eq!(
            reg.clear_pending_if_timed_out(deadline),
            Some(0),
            "REG2 wait must time out at REG2_TIMEOUT (4s)"
        );
        assert_eq!(reg.pending_reg2_idx(), None, "timeout clears pending");
        assert_eq!(
            reg.pending_timeout_at_ms(),
            0,
            "timeout clears the deadline"
        );
    }

    // handle_reg2 arms the REG3 deadline (REG3_TIMEOUT, 4s) and clears pending on
    // success; re-arming pending models "REG3 never arrived" so the same seam can be
    // driven to the REG3 boundary in logical time.
    #[tokio::test(start_paused = true)]
    async fn reg3_timeout_fires_at_4s_logical() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.set_pending_reg2_idx(Some(0));
        let mut full_id = reg.srtla_id;
        full_id[SRTLA_ID_LEN / 2..].fill(0x7e);

        let base = now_ms();
        reg.process_registration_packet(0, &create_reg2_packet(&full_id), now_ms());

        let deadline = reg.pending_timeout_at_ms();
        assert!(
            deadline >= base + REG3_TIMEOUT * 1000 && deadline <= now_ms() + REG3_TIMEOUT * 1000,
            "REG3 deadline must be REG3_TIMEOUT (4s) past the received REG2"
        );

        reg.set_pending_reg2_idx(Some(0));
        assert_eq!(
            reg.clear_pending_if_timed_out(deadline - 1),
            None,
            "must not time out before REG3_TIMEOUT"
        );
        assert_eq!(
            reg.clear_pending_if_timed_out(deadline),
            Some(0),
            "REG3 wait must time out at REG3_TIMEOUT (4s)"
        );
    }

    // ------------------------------------------------------------------
    // Registration hardening: the uplink sockets are deliberately unconnected
    // (accept-any-source), and SRTLA control frames are unauthenticated, so any
    // host that can reach an uplink's source port can forge a 2-byte REG3 or
    // REG_ERR. The manager gates both on the handshake phase that uplink
    // actually has in flight.
    //
    // The shell (`src/sender/uplink_recv.rs`) keys its connection-state effects
    // on the returned `RegistrationEvent`: `Reg3` runs
    // `clear_pre_registration_state` + `connected = true`, `RegErr` runs
    // `connected = false`. `None` means "no registration effect", so these
    // tests assert on the returned event; `apply_shell_registration_effects`
    // mirrors that dispatch to prove the end-to-end consequence on a live link.
    // ------------------------------------------------------------------

    fn reg3_packet() -> Vec<u8> {
        vec![(SRTLA_TYPE_REG3 >> 8) as u8, (SRTLA_TYPE_REG3 & 0xff) as u8]
    }

    fn reg_err_packet() -> Vec<u8> {
        vec![
            (SRTLA_TYPE_REG_ERR >> 8) as u8,
            (SRTLA_TYPE_REG_ERR & 0xff) as u8,
        ]
    }

    /// Mirror of the registration arms in `src/sender/uplink_recv.rs`. Kept in
    /// the test so a gate change is measured by its real consequence on a
    /// connection, not just by the event enum.
    fn apply_shell_registration_effects(
        event: Option<RegistrationEvent>,
        conn: &mut srtla_core::connection::SrtlaConnection,
        now: u64,
    ) {
        match event {
            Some(RegistrationEvent::Reg3) => {
                conn.clear_pre_registration_state(now);
                conn.connected = true;
                conn.last_received = Some(now);
            }
            Some(RegistrationEvent::RegErr) => {
                conn.connected = false;
                conn.last_received = None;
            }
            _ => {}
        }
    }

    // --- (a) REG3 replay ---------------------------------------------------

    /// A REG3 on an uplink we never sent a REG2 to must not promote it.
    #[tokio::test]
    async fn unsolicited_reg3_is_ignored_and_counted() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        conn.connected = false;

        let event = reg.process_registration_packet(0, &reg3_packet(), now_ms());
        assert!(event.is_none(), "an unsolicited REG3 produces no event");
        apply_shell_registration_effects(event, &mut conn, now_ms());

        assert!(
            !reg.has_connected(),
            "a REG3 with no REG2 in flight must not register anything"
        );
        assert!(!conn.connected, "the uplink must stay unregistered");
        assert_eq!(reg.out_of_phase_reg3(), 1);

        // ...and the same frame is honored once a REG2 has actually gone out.
        let _ = reg.build_reg2(0);
        let event = reg.process_registration_packet(0, &reg3_packet(), now_ms());
        assert!(matches!(event, Some(RegistrationEvent::Reg3)));
        assert!(reg.has_connected());
        assert_eq!(reg.out_of_phase_reg3(), 1, "no new rejection");
    }

    /// The grant is ONE-SHOT: a duplicate/replayed REG3 must not re-run the
    /// "just registered" effects on a live link, which would wipe its packet
    /// log, in-flight count, congestion state and batch queue.
    #[tokio::test]
    async fn replayed_reg3_does_not_wipe_a_live_connection() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        let now = now_ms();

        let _ = reg.build_reg2(0);
        assert!(reg.is_awaiting_reg3(0));
        let event = reg.process_registration_packet(0, &reg3_packet(), now);
        apply_shell_registration_effects(event, &mut conn, now);
        assert!(
            conn.connected,
            "the first in-phase REG3 registers the uplink"
        );
        assert!(
            !reg.is_awaiting_reg3(0),
            "the one-shot grant is consumed by the REG3 it authorized"
        );

        // Give the live link real in-flight state.
        conn.register_packet(7, now);
        conn.register_packet(8, now);
        assert_eq!(conn.in_flight_packets, 2);

        let event = reg.process_registration_packet(0, &reg3_packet(), now_ms());
        assert!(
            event.is_none(),
            "a replayed REG3 must not reach the shell's registration effects"
        );
        apply_shell_registration_effects(event, &mut conn, now_ms());

        assert_eq!(reg.out_of_phase_reg3(), 1);
        assert_eq!(
            conn.in_flight_packets, 2,
            "a replayed REG3 must not clear a live uplink's in-flight state"
        );
        assert_eq!(
            conn.packet_log.len(),
            2,
            "packet log must survive the replay"
        );
        assert!(conn.connected, "the link stays established");
    }

    /// A spoofed REG3 flood is the same one-shot rejection, repeated.
    #[tokio::test]
    async fn reg3_flood_leaves_a_registered_link_untouched() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        let now = now_ms();

        let _ = reg.build_reg2(0);
        let event = reg.process_registration_packet(0, &reg3_packet(), now);
        apply_shell_registration_effects(event, &mut conn, now);
        conn.register_packet(1, now);

        for _ in 0..64 {
            let event = reg.process_registration_packet(0, &reg3_packet(), now_ms());
            assert!(event.is_none());
            apply_shell_registration_effects(event, &mut conn, now_ms());
        }

        assert_eq!(reg.out_of_phase_reg3(), 64);
        assert!(conn.connected);
        assert_eq!(conn.in_flight_packets, 1);
    }

    // --- (b) REG2 re-arm ---------------------------------------------------

    /// Liveness: a reconnecting uplink's REG2 resend must re-arm the gate that
    /// its previous REG3 consumed, or the link could never re-register.
    #[tokio::test]
    async fn reconnect_reg2_rearms_the_reg3_gate() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        let now = now_ms();

        let _ = reg.build_reg2(0);
        let event = reg.process_registration_packet(0, &reg3_packet(), now);
        apply_shell_registration_effects(event, &mut conn, now);
        assert!(conn.connected);

        // The link times out; housekeeping resets it and resends REG2.
        conn.mark_for_recovery();
        assert!(!conn.connected);
        reg.update_active_connections(std::slice::from_ref(&conn));
        let _ = reg.build_reg2(0);
        assert!(
            reg.is_awaiting_reg3(0),
            "the reconnect REG2 must re-arm the one-shot grant"
        );

        let event = reg.process_registration_packet(0, &reg3_packet(), now_ms());
        assert!(
            matches!(event, Some(RegistrationEvent::Reg3)),
            "the reconnect REG3 must be honored"
        );
        apply_shell_registration_effects(event, &mut conn, now_ms());
        assert!(conn.connected, "the link re-registers");
        assert_eq!(reg.out_of_phase_reg3(), 0);
    }

    /// A REG2 broadcast must not re-arm the consumed grant of an uplink that is
    /// already established and forwarding: the REG3 it provokes would otherwise
    /// wipe that link's live state.
    #[tokio::test]
    async fn reg2_broadcast_does_not_rearm_a_connected_uplink() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![
            create_test_connection().await,
            create_test_connection().await,
        ];
        let now = now_ms();
        connections[0].connected = false;
        connections[1].connected = false;

        // Uplink 0 is live and forwarding; uplink 1 is still unregistered.
        let _ = reg.build_reg2(0);
        let event = reg.process_registration_packet(0, &reg3_packet(), now);
        apply_shell_registration_effects(event, &mut connections[0], now);
        connections[0].register_packet(42, now);
        assert!(connections[0].connected && connections[0].in_flight_packets == 1);

        reg.update_active_connections(&connections);
        reg.set_broadcast_reg2_pending(true);
        let sends = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert!(
            sends.broadcast_reg2.is_some(),
            "the broadcast still goes out"
        );
        assert!(
            !reg.is_awaiting_reg3(0),
            "the connected uplink's consumed grant must not be re-armed"
        );
        assert!(
            reg.is_awaiting_reg3(1),
            "an unregistered uplink still gets a grant"
        );

        // The receiver answers the broadcast on both links.
        let event = reg.process_registration_packet(0, &reg3_packet(), now_ms());
        apply_shell_registration_effects(event, &mut connections[0], now_ms());
        assert_eq!(
            connections[0].in_flight_packets, 1,
            "the live uplink keeps its in-flight state"
        );
        assert!(connections[0].connected);

        let event = reg.process_registration_packet(1, &reg3_packet(), now_ms());
        assert!(
            matches!(event, Some(RegistrationEvent::Reg3)),
            "uplink 1 registers normally"
        );
        apply_shell_registration_effects(event, &mut connections[1], now_ms());
        assert!(connections[1].connected);
    }

    // --- (c) REG_ERR DoS ---------------------------------------------------

    /// A forged 2-byte REG_ERR is the cheapest remote DoS: ungated it
    /// force-disconnects an established, forwarding uplink.
    #[tokio::test]
    async fn reg_err_flood_does_not_disconnect_a_registered_link() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        let now = now_ms();

        let _ = reg.build_reg2(0);
        let event = reg.process_registration_packet(0, &reg3_packet(), now);
        apply_shell_registration_effects(event, &mut conn, now);
        conn.register_packet(5, now);
        assert!(conn.connected);

        for _ in 0..64 {
            let event = reg.process_registration_packet(0, &reg_err_packet(), now_ms());
            assert!(
                event.is_none(),
                "an out-of-phase REG_ERR must not reach the shell's teardown"
            );
            apply_shell_registration_effects(event, &mut conn, now_ms());
        }

        assert!(
            conn.connected,
            "a forged REG_ERR flood must not tear down a healthy registered link"
        );
        assert_eq!(conn.in_flight_packets, 1);
        assert_eq!(reg.out_of_phase_reg_err(), 64);
        assert_eq!(reg.pending_reg2_idx(), None);
    }

    /// The REG1/REG2 fields are a single global slot: a REG_ERR arriving on
    /// uplink A must not abort uplink B's in-flight handshake.
    #[test]
    fn out_of_phase_reg_err_does_not_damage_another_uplinks_handshake() {
        let mut reg = SrtlaRegistrationManager::new();

        reg.build_reg1_for(1, now_ms());
        assert_eq!(reg.pending_reg2_idx(), Some(1));
        let deadline = reg.pending_timeout_at_ms();

        let event = reg.process_registration_packet(0, &reg_err_packet(), now_ms());
        assert!(event.is_none());

        assert_eq!(
            reg.pending_reg2_idx(),
            Some(1),
            "uplink B keeps its in-flight REG2 window"
        );
        assert_eq!(reg.reg1_target_idx(), Some(1), "uplink B keeps its target");
        assert_eq!(reg.pending_timeout_at_ms(), deadline);
        assert_eq!(reg.out_of_phase_reg_err(), 1);
    }

    /// A REG_ERR that answers a handshake actually in flight still tears that
    /// attempt down — in both the awaiting-REG2 and the awaiting-REG3 phase.
    #[tokio::test]
    async fn in_phase_reg_err_still_aborts_the_registration() {
        // Awaiting REG2.
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        conn.connected = true;
        reg.build_reg1_for(0, now_ms());

        let after = now_ms();
        let event = reg.process_registration_packet(0, &reg_err_packet(), after);
        assert!(matches!(event, Some(RegistrationEvent::RegErr)));
        apply_shell_registration_effects(event, &mut conn, after);

        assert!(!conn.connected, "an in-phase REG_ERR tears the link down");
        assert_eq!(reg.pending_reg2_idx(), None);
        assert_eq!(reg.reg1_target_idx(), None);
        assert_eq!(reg.pending_timeout_at_ms(), 0);
        assert!(reg.reg1_next_send_at_ms() >= after + REG2_TIMEOUT * 1000);
        assert_eq!(reg.out_of_phase_reg_err(), 0);

        // Awaiting REG3.
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;
        conn.connected = true;
        let _ = reg.build_reg2(0);
        assert!(reg.is_awaiting_reg3(0));

        let event = reg.process_registration_packet(0, &reg_err_packet(), now_ms());
        assert!(matches!(event, Some(RegistrationEvent::RegErr)));
        apply_shell_registration_effects(event, &mut conn, now_ms());

        assert!(!conn.connected, "a REG_ERR while awaiting REG3 also aborts");
        assert!(
            !reg.is_awaiting_reg3(0),
            "the in-phase REG_ERR revokes the grant"
        );
        assert_eq!(reg.out_of_phase_reg_err(), 0);
    }

    /// End-to-end liveness regression guard: the full REG_NGP → REG1 → REG2 →
    /// broadcast → REG3 flow still registers every uplink with the gates on,
    /// and a receiver restart (REG_ERR mid-handshake, then a fresh cycle) still
    /// recovers.
    #[tokio::test]
    async fn hardened_flow_still_registers_and_recovers() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![
            create_test_connection().await,
            create_test_connection().await,
        ];

        // Cold start: nothing is registered yet.
        for conn in connections.iter_mut() {
            conn.connected = false;
        }
        let ngp = [0x92, 0x11];
        reg.process_registration_packet(0, &ngp, now_ms());
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert_eq!(reg.pending_reg2_idx(), Some(0));

        let mut full_id = reg.srtla_id;
        full_id[SRTLA_ID_LEN / 2..].fill(0x5a);
        reg.process_registration_packet(0, &create_reg2_packet(&full_id), now_ms());
        reg.update_active_connections(&connections);
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        for (idx, conn) in connections.iter_mut().enumerate() {
            let now = now_ms();
            let event = reg.process_registration_packet(idx, &reg3_packet(), now);
            assert!(
                matches!(event, Some(RegistrationEvent::Reg3)),
                "uplink {idx} must register on the cold-start flow"
            );
            apply_shell_registration_effects(event, conn, now);
        }
        assert!(connections.iter().all(|c| c.connected));
        reg.update_active_connections(&connections);
        assert_eq!(reg.active_connections(), 2);

        // Receiver restart: every link drops, housekeeping resets them and the
        // whole handshake runs again from a fresh REG_NGP.
        for conn in connections.iter_mut() {
            conn.mark_for_recovery();
        }
        reg.update_active_connections(&connections);
        assert_eq!(reg.active_connections(), 0);

        reg.process_registration_packet(1, &ngp, now_ms());
        assert_eq!(reg.reg1_target_idx(), Some(1));
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());
        assert_eq!(reg.pending_reg2_idx(), Some(1));

        let mut full_id = reg.srtla_id;
        full_id[SRTLA_ID_LEN / 2..].fill(0x7e);
        reg.process_registration_packet(1, &create_reg2_packet(&full_id), now_ms());
        let _ = reg.reg_driver_pending_sends(connections.len(), now_ms());

        for (idx, conn) in connections.iter_mut().enumerate() {
            let now = now_ms();
            let event = reg.process_registration_packet(idx, &reg3_packet(), now);
            assert!(
                matches!(event, Some(RegistrationEvent::Reg3)),
                "uplink {idx} must re-register after the receiver restart"
            );
            apply_shell_registration_effects(event, conn, now);
        }
        assert!(connections.iter().all(|c| c.connected));
        assert_eq!(reg.out_of_phase_reg3(), 0);
        assert_eq!(reg.out_of_phase_reg_err(), 0);
    }

    // A fresh link (last_received == None, not yet connected) drives registration,
    // never reconnection, while it is inside its startup grace window. is_timed_out
    // compares now_ms() against the grace deadline, so the test picks the deadline
    // rather than advancing a virtual clock.
    #[tokio::test]
    async fn fresh_link_not_timed_out() {
        let mut conn = create_test_connection().await;

        conn.connected = false;
        conn.last_received = None;
        conn.reconnection.connection_established_ms = 0;

        // Within the grace window: never-received link is not timed out, so
        // housekeeping keeps driving registration instead of reconnection.
        conn.reconnection.startup_grace_deadline_ms = now_ms() + STARTUP_GRACE_MS;
        assert!(
            !conn.is_timed_out(now_ms()),
            "fresh never-received link within grace must NOT be timed out"
        );

        // Past the grace deadline: a never-established link that never received
        // data is now timed out, which is what drives re-registration.
        conn.reconnection.startup_grace_deadline_ms = now_ms().saturating_sub(1);
        assert!(
            conn.is_timed_out(now_ms()),
            "a fresh link past its startup grace deadline must be timed out"
        );
    }
}
