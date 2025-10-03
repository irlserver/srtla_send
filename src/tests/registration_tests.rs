#[cfg(test)]
mod tests {

    use crate::protocol::*;
    use crate::registration::*;
    use crate::test_helpers::create_test_connection;
    use crate::utils::now_ms;

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
        let handled = reg.process_registration_packet(1, &buf);
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

        let handled = reg.process_registration_packet(0, &buf);
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

        let handled = reg.process_registration_packet(2, &buf);
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

        let handled = reg.process_registration_packet(1, &buf);
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

        let handled = reg.process_registration_packet(0, &buf);
        assert!(handled.is_none());
    }

    #[tokio::test]
    async fn test_reg_driver_initial_reg1() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![create_test_connection().await];

        let mut ngp = vec![0u8; 2];
        ngp[0..2].copy_from_slice(&SRTLA_TYPE_REG_NGP.to_be_bytes());
        reg.process_registration_packet(0, &ngp);

        // Should send REG1 to first connection when no connections are active
        reg.reg_driver_send_if_needed(&mut connections).await;

        assert_eq!(reg.pending_reg2_idx(), Some(0));
        assert!(reg.pending_timeout_at_ms() > now_ms());
    }

    #[tokio::test]
    async fn test_reg_driver_with_target() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![
            create_test_connection().await,
            create_test_connection().await,
        ];

        // Set a specific target from REG_NGP
        reg.set_reg1_target_idx(Some(1));

        reg.reg_driver_send_if_needed(&mut connections).await;

        // Should send to the specified target
        assert_eq!(reg.pending_reg2_idx(), Some(1));
    }

    #[tokio::test]
    async fn test_reg_driver_waits_for_ngp() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![create_test_connection().await];

        // Without REG_NGP, nothing should happen
        reg.reg_driver_send_if_needed(&mut connections).await;
        assert_eq!(reg.pending_reg2_idx(), None);

        let mut ngp = vec![0u8; 2];
        ngp[0..2].copy_from_slice(&SRTLA_TYPE_REG_NGP.to_be_bytes());
        reg.process_registration_packet(0, &ngp);

        reg.reg_driver_send_if_needed(&mut connections).await;
        assert_eq!(reg.pending_reg2_idx(), Some(0));
    }

    #[tokio::test]
    async fn test_broadcast_reg2() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut connections = vec![
            create_test_connection().await,
            create_test_connection().await,
        ];

        // Trigger broadcast
        reg.set_broadcast_reg2_pending(true);
        reg.reg_driver_send_if_needed(&mut connections).await;

        // Should have cleared the broadcast flag
        assert!(!reg.broadcast_reg2_pending());
    }

    #[tokio::test]
    async fn test_send_reg1_to_sets_pending_state() {
        let mut reg = SrtlaRegistrationManager::new();
        let mut conn = create_test_connection().await;

        reg.send_reg1_to(0, &mut conn).await;

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
        let mut conn = create_test_connection().await;

        // Pretend we already have pending state for another index
        reg.set_pending_reg2_idx(Some(1));
        reg.set_reg1_target_idx(Some(1));

        reg.send_reg2_to(0, &mut conn).await;

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

        // Simulate multiple REG3 responses
        for i in 0..3 {
            let handled = reg.process_registration_packet(i, &reg3_packet);
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
        let mut connections = vec![create_test_connection().await];
        reg.reg_driver_send_if_needed(&mut connections).await;
        assert_eq!(
            reg.pending_reg2_idx(),
            None,
            "Should not send before next-send time"
        );
    }

    #[tokio::test]
    async fn test_registration_state_transitions() {
        let mut reg = SrtlaRegistrationManager::new();
        let connections = vec![create_test_connection().await];

        // Initial state
        assert_eq!(reg.active_connections(), 0);
        assert!(!reg.has_connected);

        // After REG_NGP
        let ngp_packet = [0x92, 0x11, 0x00, 0x00];
        reg.process_registration_packet(0, &ngp_packet);
        assert_eq!(reg.reg1_target_idx(), Some(0));

        // Set up for REG2
        reg.set_pending_reg2_idx(Some(0));

        // Process REG2
        let mut modified_id = reg.srtla_id;
        modified_id[SRTLA_ID_LEN / 2..].fill(0xff);
        let reg2_packet = create_reg2_packet(&modified_id);
        reg.process_registration_packet(0, &reg2_packet);

        assert!(reg.broadcast_reg2_pending());
        assert_eq!(reg.pending_reg2_idx(), None);

        // Process REG3
        let reg3_packet = vec![0x92, 0x02];
        reg.process_registration_packet(0, &reg3_packet);

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
}
