use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::protocol::{
    DRIFT_TOLERANCE_SECONDS, DriftCorrectionAction, HARD_DRIFT_THRESHOLD_SECONDS, PlaybackState,
    PlaybackStatus,
};

#[derive(Debug, Clone)]
pub struct AuthoritativePlaybackClock {
    status: PlaybackStatus,
    anchor_position_seconds: f64,
    updated_at: OffsetDateTime,
    playback_rate: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct PlaybackClockCheckpoint {
    pub status: PlaybackStatus,
    pub anchor_position_seconds: f64,
    pub updated_at: OffsetDateTime,
    pub playback_rate: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DriftReport {
    pub expected_position_seconds: f64,
    pub reported_position_seconds: f64,
    pub delta_seconds: f64,
    pub tolerance_seconds: f64,
    pub suggested_action: DriftCorrectionAction,
}

impl AuthoritativePlaybackClock {
    pub fn new_paused(now: OffsetDateTime) -> Self {
        Self {
            status: PlaybackStatus::Paused,
            anchor_position_seconds: 0.0,
            updated_at: now,
            playback_rate: 1.0,
        }
    }

    pub fn restore(checkpoint: PlaybackClockCheckpoint) -> Self {
        Self {
            status: checkpoint.status,
            anchor_position_seconds: checkpoint.anchor_position_seconds,
            updated_at: checkpoint.updated_at,
            playback_rate: checkpoint.playback_rate,
        }
    }

    pub fn snapshot(&self, emitted_at: OffsetDateTime) -> PlaybackState {
        PlaybackState {
            status: self.status,
            position_seconds: round_to(self.effective_position_at(emitted_at), 1),
            anchor_position_seconds: round_to(self.anchor_position_seconds, 1),
            clock_updated_at: format_timestamp(self.updated_at),
            emitted_at: format_timestamp(emitted_at),
            playback_rate: self.playback_rate,
            drift_tolerance_seconds: DRIFT_TOLERANCE_SECONDS,
        }
    }

    pub fn checkpoint(&self) -> PlaybackClockCheckpoint {
        PlaybackClockCheckpoint {
            status: self.status,
            anchor_position_seconds: self.anchor_position_seconds,
            updated_at: self.updated_at,
            playback_rate: self.playback_rate,
        }
    }

    pub fn play(&mut self, now: OffsetDateTime, position_seconds: Option<f64>) {
        self.anchor_position_seconds =
            position_seconds.unwrap_or_else(|| self.effective_position_at(now));
        self.updated_at = now;
        self.status = PlaybackStatus::Playing;
    }

    pub fn pause(&mut self, now: OffsetDateTime, position_seconds: Option<f64>) {
        self.anchor_position_seconds =
            position_seconds.unwrap_or_else(|| self.effective_position_at(now));
        self.updated_at = now;
        self.status = PlaybackStatus::Paused;
    }

    pub fn seek(&mut self, now: OffsetDateTime, position_seconds: f64) {
        self.anchor_position_seconds = position_seconds;
        self.updated_at = now;
    }

    pub fn report_drift(&self, now: OffsetDateTime, reported_position_seconds: f64) -> DriftReport {
        let expected_position_seconds = self.effective_position_at(now);
        let delta_seconds = reported_position_seconds - expected_position_seconds;
        let absolute_delta = delta_seconds.abs();

        let suggested_action = if absolute_delta <= DRIFT_TOLERANCE_SECONDS {
            DriftCorrectionAction::InSync
        } else if absolute_delta <= HARD_DRIFT_THRESHOLD_SECONDS {
            DriftCorrectionAction::Nudge
        } else {
            DriftCorrectionAction::Seek
        };

        DriftReport {
            expected_position_seconds: round_to(expected_position_seconds, 3),
            reported_position_seconds: round_to(reported_position_seconds, 3),
            delta_seconds: round_to(delta_seconds, 3),
            tolerance_seconds: DRIFT_TOLERANCE_SECONDS,
            suggested_action,
        }
    }

    fn effective_position_at(&self, now: OffsetDateTime) -> f64 {
        match self.status {
            PlaybackStatus::Paused => self.anchor_position_seconds,
            PlaybackStatus::Playing => {
                let elapsed_seconds = (now - self.updated_at).as_seconds_f64().max(0.0);
                self.anchor_position_seconds + (elapsed_seconds * self.playback_rate)
            }
        }
    }
}

pub(crate) fn format_timestamp(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&Rfc3339)
        .expect("RFC3339 formatting should succeed")
}

pub(crate) fn round_to(value: f64, decimals: u32) -> f64 {
    let multiplier = 10_f64.powi(decimals as i32);
    (value * multiplier).round() / multiplier
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PlaybackStatus;
    use time::Duration;

    #[test]
    fn playback_clock_advances_when_playing() {
        let start = OffsetDateTime::UNIX_EPOCH;
        let mut clock = AuthoritativePlaybackClock::new_paused(start);
        clock.play(start, Some(12.0));

        let snapshot = clock.snapshot(start + Duration::milliseconds(240));

        assert_eq!(snapshot.status, PlaybackStatus::Playing);
        assert_eq!(snapshot.position_seconds, 12.2);
    }

    #[test]
    fn drift_report_classifies_thresholds() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut clock = AuthoritativePlaybackClock::new_paused(now);
        clock.play(now, Some(10.0));

        let in_sync = clock.report_drift(now, 10.2);
        assert_eq!(in_sync.suggested_action, DriftCorrectionAction::InSync);

        let nudge = clock.report_drift(now, 10.8);
        assert_eq!(nudge.suggested_action, DriftCorrectionAction::Nudge);

        let seek = clock.report_drift(now, 12.1);
        assert_eq!(seek.suggested_action, DriftCorrectionAction::Seek);
    }

    #[test]
    fn playback_clock_does_not_move_backwards_before_anchor_time() {
        let start = OffsetDateTime::UNIX_EPOCH + Duration::seconds(5);
        let mut clock = AuthoritativePlaybackClock::new_paused(start);
        clock.play(start, Some(7.0));

        let snapshot = clock.snapshot(start - Duration::milliseconds(150));

        assert_eq!(snapshot.position_seconds, 7.0);
        assert_eq!(snapshot.anchor_position_seconds, 7.0);
    }

    #[test]
    fn drift_report_treats_negative_delta_symmetrically() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut clock = AuthoritativePlaybackClock::new_paused(now);
        clock.play(now, Some(10.0));

        let behind = clock.report_drift(now, 8.9);

        assert_eq!(behind.suggested_action, DriftCorrectionAction::Nudge);
        assert!(behind.delta_seconds < 0.0);
    }
}
