use std::time::{Duration, Instant};

pub(super) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HealthAction {
    None,
    Ping,
    Expired,
}

pub(super) struct EndpointHealth {
    connected_at: Instant,
    last_received: Instant,
    ping_sent_at: Option<Instant>,
    ready: bool,
}

impl EndpointHealth {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            connected_at: now,
            last_received: now,
            ping_sent_at: None,
            ready: false,
        }
    }

    pub(super) fn received(&mut self, now: Instant) {
        self.last_received = now;
        self.ping_sent_at = None;
    }

    pub(super) fn ready(&mut self) {
        self.ready = true;
    }

    pub(super) fn action(&self, now: Instant) -> HealthAction {
        let initial_snapshot_expired =
            !self.ready && now.saturating_duration_since(self.connected_at) >= HEARTBEAT_TIMEOUT;
        let probe_expired = self
            .ping_sent_at
            .is_some_and(|sent_at| now.saturating_duration_since(sent_at) >= HEARTBEAT_TIMEOUT);
        if initial_snapshot_expired || probe_expired {
            HealthAction::Expired
        } else if self.ping_sent_at.is_none()
            && now.saturating_duration_since(self.last_received) >= HEARTBEAT_INTERVAL
        {
            HealthAction::Ping
        } else {
            HealthAction::None
        }
    }

    pub(super) fn ping_sent(&mut self, now: Instant) {
        self.ping_sent_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_connection_is_probed_then_expires_without_a_reply() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        assert_eq!(health.action(now), HealthAction::None);
        assert_eq!(health.action(now + HEARTBEAT_INTERVAL), HealthAction::Ping);
        health.ping_sent(now + HEARTBEAT_INTERVAL);
        assert_eq!(
            health.action(now + HEARTBEAT_INTERVAL + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }

    #[test]
    fn any_incoming_message_satisfies_an_outstanding_probe() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        health.ping_sent(now);
        health.received(now + HEARTBEAT_TIMEOUT - Duration::from_millis(1));
        assert_eq!(health.action(now + HEARTBEAT_TIMEOUT), HealthAction::None);
    }

    #[test]
    fn a_probe_queued_behind_a_draining_frame_never_expires_the_connection() {
        // bd herdr-7ak: the ping sits in the writer queue behind a paste that is still being
        // accepted by the peer, so the endpoint is provably alive and no reply is owed yet.
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        let receipt = TransmitReceipt::pending();
        health.ping_queued(receipt.clone());
        health.settle();
        assert!(health.is_draining());
        assert_eq!(health.action(now + HEARTBEAT_INTERVAL), HealthAction::None);
        assert_eq!(
            health.action(now + HEARTBEAT_INTERVAL + HEARTBEAT_TIMEOUT),
            HealthAction::None,
            "a queued ping expired the connection while its frame was still draining"
        );
        assert_eq!(
            health.action(now + Duration::from_secs(300)),
            HealthAction::None
        );
    }

    #[test]
    fn the_probe_timer_starts_when_the_writer_accepted_the_ping() {
        // The 1 MiB paste needs about 20 s of drain; the reply window opens at the instant the
        // writer took the ping's last byte, not at the instant the ping was queued behind it.
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        let receipt = TransmitReceipt::pending();
        health.ping_queued(receipt.clone());
        let transmitted = now + Duration::from_secs(20);
        receipt.complete(transmitted);
        health.settle();
        assert!(!health.is_draining());
        assert_eq!(
            health.action(transmitted + HEARTBEAT_TIMEOUT - Duration::from_millis(1)),
            HealthAction::None
        );
        assert_eq!(
            health.action(transmitted + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }

    #[test]
    fn a_reply_satisfies_a_ping_that_is_still_queued() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        health.ping_queued(TransmitReceipt::pending());
        health.received(now + Duration::from_secs(1));
        assert!(!health.is_draining());
        assert_eq!(
            health.action(now + Duration::from_secs(1)),
            HealthAction::None
        );
        assert_eq!(
            health.action(now + Duration::from_secs(1) + HEARTBEAT_INTERVAL),
            HealthAction::Ping,
            "a satisfied probe must leave the endpoint pingable again"
        );
    }

    #[test]
    fn heartbeats_do_not_hide_a_missing_initial_snapshot() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ping_sent(now + HEARTBEAT_INTERVAL);
        health.received(now + HEARTBEAT_INTERVAL + Duration::from_secs(1));
        assert_eq!(
            health.action(now + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }
}
