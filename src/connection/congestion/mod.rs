//! Congestion control strategies for SRTLA connections
//!
//! This module provides two congestion control strategies:
//!
//! ## Classic Mode
//! Matches the original C implementation exactly:
//! - Simple window increase based on in-flight packets
//! - No time-based recovery
//! - No fast recovery mode
//!
//! ## Enhanced Mode
//! Enhanced window management with:
//! - Same base growth as classic (prevents thrashing)
//! - Fast recovery mode for severe congestion
//! - Time-based progressive window recovery
//! - NAK burst tracking

pub mod classic;
pub mod enhanced;

use tracing::warn;

use crate::protocol::*;
use crate::utils::now_ms;

const NAK_BURST_WINDOW_MS: u64 = 1000;
const NAK_BURST_LOG_THRESHOLD: i32 = 10;

/// Congestion control and NAK tracking state
#[derive(Debug, Clone, Default)]
pub struct CongestionControl {
    pub nak_count: i32,
    pub last_nak_time_ms: u64,
    pub last_window_increase_ms: u64,
    pub consecutive_acks_without_nak: i32,
    pub fast_recovery_mode: bool,
    pub fast_recovery_start_ms: u64,
    pub nak_burst_count: i32,
    pub nak_burst_start_time_ms: u64,
}

impl CongestionControl {
    /// Reset all congestion control state to initial values
    /// Used during reconnection to start with a clean slate
    pub fn reset(&mut self) {
        self.nak_count = 0;
        self.last_nak_time_ms = 0;
        self.nak_burst_count = 0;
        self.nak_burst_start_time_ms = 0;
        self.last_window_increase_ms = 0;
        self.consecutive_acks_without_nak = 0;
        self.fast_recovery_mode = false;
        self.fast_recovery_start_ms = 0;
    }

    /// Handle NAK reception (common to both classic and enhanced)
    ///
    /// Returns true if the NAK was handled successfully
    pub fn handle_nak(&mut self, window: &mut i32, seq: i32, label: &str) -> bool {
        let current_time = now_ms();
        self.nak_count = self.nak_count.saturating_add(1);

        let time_since_last_nak = current_time.saturating_sub(self.last_nak_time_ms);

        // Track NAK bursts
        if self.last_nak_time_ms > 0 && time_since_last_nak < NAK_BURST_WINDOW_MS {
            if self.nak_burst_count == 0 {
                self.nak_burst_count = 2;
                self.nak_burst_start_time_ms = self.last_nak_time_ms;
            } else {
                self.nak_burst_count = self.nak_burst_count.saturating_add(1);
            }
        } else {
            if self.nak_burst_count >= NAK_BURST_LOG_THRESHOLD {
                let burst_duration = current_time.saturating_sub(self.nak_burst_start_time_ms);
                warn!(
                    "{}: NAK burst ended - {} NAKs in {}ms",
                    label, self.nak_burst_count, burst_duration
                );
            }
            self.nak_burst_count = 0;
            self.nak_burst_start_time_ms = 0;
        }

        self.last_nak_time_ms = current_time;
        self.consecutive_acks_without_nak = 0;

        // Reduce window
        let old_window = *window;
        *window = (*window - WINDOW_DECR).max(WINDOW_MIN * WINDOW_MULT);

        if *window <= 3000 {
            let burst_info = if self.nak_burst_count > 1 {
                format!(" [BURST: {} NAKs]", self.nak_burst_count)
            } else {
                String::new()
            };
            warn!(
                "{}: NAK reduced window {} → {} (seq={}, total_naks={}{})",
                label, old_window, *window, seq, self.nak_count, burst_info
            );
        }

        // Enter fast recovery mode if window is very low
        if *window <= 2000 && !self.fast_recovery_mode {
            self.fast_recovery_mode = true;
            self.fast_recovery_start_ms = current_time;
            warn!(
                "{}: Enabling FAST RECOVERY MODE - window {}",
                label, *window
            );
        }

        true
    }

    /// Handle SRTLA ACK specific to this connection
    ///
    /// Dispatches to classic or enhanced implementation based on mode
    pub fn handle_srtla_ack_specific_classic(
        &mut self,
        window: &mut i32,
        in_flight_packets: i32,
        seq: i32,
        label: &str,
    ) {
        classic::handle_srtla_ack_specific(window, in_flight_packets, seq, label);
    }

    /// Handle SRTLA ACK with enhanced algorithm
    pub fn handle_srtla_ack_enhanced(
        &mut self,
        window: &mut i32,
        in_flight_packets: i32,
        label: &str,
    ) {
        enhanced::handle_srtla_ack(
            window,
            in_flight_packets,
            &mut self.fast_recovery_mode,
            self.fast_recovery_start_ms,
            label,
        );
    }

    /// Perform window recovery (enhanced mode only)
    pub fn perform_window_recovery(&mut self, window: &mut i32, connected: bool, label: &str) {
        enhanced::perform_window_recovery(
            window,
            connected,
            self.last_nak_time_ms,
            &mut self.nak_burst_count,
            &mut self.nak_burst_start_time_ms,
            &mut self.last_window_increase_ms,
            &mut self.fast_recovery_mode,
            label,
        );
    }

    /// Get time since last NAK in milliseconds
    pub fn time_since_last_nak_ms(&self) -> Option<u64> {
        if self.last_nak_time_ms == 0 {
            None
        } else {
            Some(now_ms().saturating_sub(self.last_nak_time_ms))
        }
    }
}
