use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use super::health::{EndpointHealth, HealthAction};
use super::ClientEndpointId;
use crate::protocol::ClientMessage;

/// When a transport accepted the last byte of one frame. A queued writer resolves it from its
/// worker thread, so the caller can tell a frame that is still draining from one that is on the
/// wire (bd herdr-7ak).
#[derive(Clone, Debug)]
pub(crate) struct TransmitReceipt(Arc<OnceLock<Instant>>);

impl TransmitReceipt {
    /// A frame still in the writer queue.
    pub(crate) fn pending() -> Self {
        Self(Arc::new(OnceLock::new()))
    }

    /// A frame already written by the time the caller's clock read `at`.
    pub(crate) fn transmitted(at: Instant) -> Self {
        let receipt = Self::pending();
        receipt.complete(at);
        receipt
    }

    /// Records the instant the writer accepted the frame's last byte. Later calls are ignored:
    /// a frame is transmitted once.
    pub(crate) fn complete(&self, at: Instant) {
        let _ = self.0.set(at);
    }

    pub(crate) fn transmitted_at(&self) -> Option<Instant> {
        self.0.get().copied()
    }
}

pub(crate) trait EndpointTransport: Send {
    fn send(&mut self, message: &ClientMessage) -> io::Result<()>;

    /// Sends `message` and reports when its last byte reaches the peer. A transport that writes
    /// inline is done when it returns, so it reports the caller's clock; a queued writer resolves
    /// the receipt from its worker thread. The health probe timer runs from that instant, never
    /// from the enqueue: a ping queued behind a 1 MiB paste can wait about 20 s for a writer that
    /// drains in 512-byte pieces, longer than the whole health window (bd herdr-7ak).
    fn send_tracked(
        &mut self,
        message: &ClientMessage,
        sent_at: Instant,
    ) -> io::Result<TransmitReceipt> {
        self.send(message)?;
        Ok(TransmitReceipt::transmitted(sent_at))
    }

    fn disconnect(&mut self) {}

    fn flush(&mut self, _deadline: Instant) -> io::Result<()> {
        Ok(())
    }

    fn take_error(&mut self) -> Option<io::Error> {
        None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointNegotiation {
    methods: HashSet<String>,
    capabilities: HashSet<String>,
}

impl EndpointNegotiation {
    pub(crate) fn new(methods: Vec<String>, capabilities: Vec<String>) -> Self {
        Self {
            methods: methods.into_iter().collect(),
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.methods.iter().cloned().collect()
    }

    pub(crate) fn supports_method(&self, method: &str) -> bool {
        self.methods.contains(method)
    }

    pub(crate) fn supports_capability(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    pub(crate) fn supports_surface_interest(&self) -> bool {
        self.supports_capability(crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY)
            && self.supports_capability(
                crate::protocol::endpoint::PRESENTATION_EFFECTS_FENCE_CAPABILITY,
            )
            && self.supports_method("client_shell.surface.set")
    }

    pub(crate) fn supports_health_check(&self) -> bool {
        self.supports_capability(crate::protocol::endpoint::HEALTH_CHECK_CAPABILITY)
    }
}

pub(crate) struct EndpointConnection {
    transport: Box<dyn EndpointTransport>,
    pub(crate) generation: u64,
    pub(crate) surface_active: bool,
    pub(crate) negotiation: EndpointNegotiation,
    health: Option<EndpointHealth>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndpointTransportFailure {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) generation: u64,
    pub(crate) kind: io::ErrorKind,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointSendOutcome {
    Sent,
    NotSent,
}

pub(crate) struct EndpointRegistry {
    active: ClientEndpointId,
    input_enabled: bool,
    connections: HashMap<ClientEndpointId, EndpointConnection>,
    failures: Vec<EndpointTransportFailure>,
}

impl EndpointRegistry {
    pub(crate) fn empty() -> Self {
        Self {
            active: ClientEndpointId::Local,
            input_enabled: false,
            connections: HashMap::new(),
            failures: Vec::new(),
        }
    }

    pub(crate) fn new(
        local: impl EndpointTransport + 'static,
        generation: u64,
        negotiation: EndpointNegotiation,
    ) -> Self {
        let mut registry = Self::empty();
        registry.input_enabled = true;
        registry.insert(
            ClientEndpointId::Local,
            local,
            generation,
            negotiation,
            true,
        );
        registry
    }

    pub(crate) fn active_id(&self) -> &ClientEndpointId {
        &self.active
    }

    pub(crate) fn active_surface_available(&self) -> bool {
        self.input_enabled
            && self
                .connections
                .get(&self.active)
                .is_some_and(|connection| connection.surface_active)
    }

    pub(crate) fn select_unavailable_local(&mut self) {
        self.active = ClientEndpointId::Local;
        self.freeze_input();
    }

    pub(crate) fn freeze_input(&mut self) {
        self.input_enabled = false;
    }

    pub(crate) fn unfreeze_input(&mut self) {
        self.input_enabled = true;
    }

    pub(crate) fn connection(&self, endpoint_id: &ClientEndpointId) -> Option<&EndpointConnection> {
        self.connections.get(endpoint_id)
    }

    pub(crate) fn insert(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: impl EndpointTransport + 'static,
        generation: u64,
        negotiation: EndpointNegotiation,
        surface_active: bool,
    ) {
        let health = (!endpoint_id.is_local() && negotiation.supports_health_check())
            .then(|| EndpointHealth::new(Instant::now()));
        if let Some(mut previous) = self.connections.insert(
            endpoint_id,
            EndpointConnection {
                transport: Box::new(transport),
                generation,
                surface_active,
                negotiation,
                health,
            },
        ) {
            previous.transport.disconnect();
        }
    }

    pub(crate) fn accepts(&self, endpoint_id: &ClientEndpointId, generation: u64) -> bool {
        self.connections
            .get(endpoint_id)
            .is_some_and(|connection| connection.generation == generation)
    }

    pub(crate) fn received(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        now: Instant,
    ) {
        if let Some(health) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
            .and_then(|connection| connection.health.as_mut())
        {
            health.received(now);
        }
    }

    pub(crate) fn mark_ready(&mut self, endpoint_id: &ClientEndpointId, generation: u64) {
        if let Some(health) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
            .and_then(|connection| connection.health.as_mut())
        {
            health.ready();
        }
    }

    pub(crate) fn tick_health(&mut self, now: Instant) {
        for connection in self.connections.values_mut() {
            if let Some(health) = connection.health.as_mut() {
                health.settle();
            }
        }
        let actions = self
            .connections
            .iter()
            .filter_map(|(endpoint_id, connection)| {
                connection
                    .health
                    .as_ref()
                    .map(|health| (endpoint_id.clone(), health.action(now)))
            })
            .filter(|(_, action)| *action != HealthAction::None)
            .collect::<Vec<_>>();
        for (endpoint_id, action) in actions {
            match action {
                HealthAction::None => {}
                HealthAction::Ping => {
                    let ping = ClientMessage::EndpointControl {
                        kind: crate::protocol::endpoint::HEALTH_PING_KIND.into(),
                        data: String::new(),
                    };
                    let Some(receipt) =
                        self.dispatch(&endpoint_id, |transport| transport.send_tracked(&ping, now))
                    else {
                        continue;
                    };
                    if let Some(health) = self
                        .connections
                        .get_mut(&endpoint_id)
                        .and_then(|connection| connection.health.as_mut())
                    {
                        health.ping_queued(receipt);
                    }
                }
                HealthAction::Expired => self.record_failure(
                    endpoint_id,
                    io::Error::new(io::ErrorKind::TimedOut, "endpoint health check timed out"),
                ),
            }
        }
    }

    pub(crate) fn set_active(&mut self, endpoint_id: &ClientEndpointId) -> bool {
        if !self
            .connections
            .get(endpoint_id)
            .is_some_and(|connection| connection.surface_active)
        {
            return false;
        }
        self.active = endpoint_id.clone();
        true
    }

    pub(crate) fn set_surface_active(
        &mut self,
        endpoint_id: &ClientEndpointId,
        active: bool,
    ) -> bool {
        let Some(connection) = self.connections.get_mut(endpoint_id) else {
            return false;
        };
        let changed = connection.surface_active != active;
        connection.surface_active = active;
        changed
    }

    pub(crate) fn send(&mut self, message: &ClientMessage) -> EndpointSendOutcome {
        let endpoint_id = self.active.clone();
        self.send_to(&endpoint_id, message)
    }

    pub(crate) fn send_to(
        &mut self,
        endpoint_id: &ClientEndpointId,
        message: &ClientMessage,
    ) -> EndpointSendOutcome {
        match self.dispatch(endpoint_id, |transport| transport.send(message)) {
            Some(()) => EndpointSendOutcome::Sent,
            None => EndpointSendOutcome::NotSent,
        }
    }

    /// Runs one transport call against a connection, revoking it on failure. Every path that
    /// writes to an endpoint goes through here, so a health ping fails it exactly like input.
    fn dispatch<T>(
        &mut self,
        endpoint_id: &ClientEndpointId,
        call: impl FnOnce(&mut dyn EndpointTransport) -> io::Result<T>,
    ) -> Option<T> {
        let result = self
            .connections
            .get_mut(endpoint_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "endpoint is unavailable"))
            .and_then(|connection| call(connection.transport.as_mut()));
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.record_failure(endpoint_id.clone(), error);
                None
            }
        }
    }

    pub(crate) fn disconnect(&mut self, endpoint_id: &ClientEndpointId) {
        self.failures
            .retain(|failure| &failure.endpoint_id != endpoint_id);
        if let Some(mut connection) = self.connections.remove(endpoint_id) {
            connection.transport.disconnect();
        }
    }

    pub(crate) fn fail(&mut self, endpoint_id: &ClientEndpointId, error: io::Error) {
        self.record_failure(endpoint_id.clone(), error);
    }

    pub(crate) fn take_failures(&mut self) -> Vec<EndpointTransportFailure> {
        let errors = self
            .connections
            .iter_mut()
            .filter_map(|(id, connection)| {
                connection
                    .transport
                    .take_error()
                    .map(|error| (id.clone(), error))
            })
            .collect::<Vec<_>>();
        for (endpoint_id, error) in errors {
            self.record_failure(endpoint_id, error);
        }
        std::mem::take(&mut self.failures)
    }

    fn record_failure(&mut self, endpoint_id: ClientEndpointId, error: io::Error) {
        let Some(generation) = self
            .connections
            .get(&endpoint_id)
            .map(|connection| connection.generation)
        else {
            return;
        };
        let failure = EndpointTransportFailure {
            endpoint_id: endpoint_id.clone(),
            generation,
            kind: error.kind(),
            message: error.to_string(),
        };
        if let Some(mut connection) = self.connections.remove(&endpoint_id) {
            connection.transport.disconnect();
        }
        if let Some(existing) = self
            .failures
            .iter_mut()
            .find(|existing| existing.endpoint_id == endpoint_id)
        {
            *existing = failure;
        } else {
            self.failures.push(failure);
        }
    }
}

impl Drop for EndpointRegistry {
    fn drop(&mut self) {
        let deadline = Instant::now() + std::time::Duration::from_millis(250);
        for connection in self.connections.values_mut() {
            let _ = connection.transport.send(&ClientMessage::Detach);
        }
        for connection in self.connections.values_mut() {
            let _ = connection.transport.flush(deadline);
            connection.transport.disconnect();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FakeTransport {
        sent: Arc<Mutex<Vec<ClientMessage>>>,
        error: Option<io::ErrorKind>,
    }

    impl EndpointTransport for FakeTransport {
        fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
            if let Some(kind) = self.error {
                return Err(io::Error::new(kind, "fake transport failure"));
            }
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }
    }

    fn negotiation() -> EndpointNegotiation {
        EndpointNegotiation::new(
            vec!["client_shell.surface.set".into()],
            vec![
                crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY.into(),
                crate::protocol::endpoint::PRESENTATION_EFFECTS_FENCE_CAPABILITY.into(),
                crate::protocol::endpoint::HEALTH_CHECK_CAPABILITY.into(),
            ],
        )
    }

    fn profile() -> crate::client::endpoint::ProfileId {
        crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    #[test]
    fn endpoint_failures_do_not_remove_other_connections() {
        let local_sent = Arc::new(Mutex::new(Vec::new()));
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: local_sent.clone(),
                error: None,
            },
            1,
            negotiation(),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: Some(io::ErrorKind::BrokenPipe),
            },
            2,
            negotiation(),
            true,
        );
        assert!(registry.set_active(&ssh_id));

        assert_eq!(
            registry.send(&ClientMessage::ClientShellFocus { focused: true }),
            EndpointSendOutcome::NotSent
        );
        assert!(registry.connection(&ssh_id).is_none());
        assert!(registry.connection(&ClientEndpointId::Local).is_some());
        assert_eq!(registry.take_failures()[0].endpoint_id, ssh_id);

        assert!(registry.set_active(&ClientEndpointId::Local));
        assert_eq!(
            registry.send(&ClientMessage::ClientShellFocus { focused: true }),
            EndpointSendOutcome::Sent
        );
        assert_eq!(local_sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn reconnecting_active_identity_does_not_count_as_an_active_surface() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            1,
            negotiation(),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            2,
            negotiation(),
            true,
        );
        assert!(registry.set_active(&ssh_id));
        assert!(registry.active_surface_available());
        registry.disconnect(&ssh_id);
        registry.insert(
            ssh_id,
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            3,
            negotiation(),
            false,
        );
        assert!(!registry.active_surface_available());
    }

    #[test]
    fn recovered_local_uses_transport_failure_not_remote_health_probes() {
        let mut registry = EndpointRegistry::empty();
        let sent = Arc::new(Mutex::new(Vec::new()));
        registry.insert(
            ClientEndpointId::Local,
            FakeTransport {
                sent: sent.clone(),
                error: None,
            },
            2,
            negotiation(),
            false,
        );
        registry.tick_health(Instant::now() + std::time::Duration::from_secs(300));
        assert!(registry.connection(&ClientEndpointId::Local).is_some());
        assert!(sent.lock().unwrap().is_empty());
        assert!(registry.take_failures().is_empty());
    }

    #[test]
    fn negotiated_remote_health_probe_expires_the_connection() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            1,
            negotiation(),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        let sent = Arc::new(Mutex::new(Vec::new()));
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: sent.clone(),
                error: None,
            },
            2,
            negotiation(),
            false,
        );
        let now = Instant::now();
        registry.tick_health(now + super::super::health::HEARTBEAT_INTERVAL);
        assert!(matches!(
            sent.lock().unwrap().as_slice(),
            [ClientMessage::EndpointControl { kind, .. }]
                if kind == crate::protocol::endpoint::HEALTH_PING_KIND
        ));

        registry.tick_health(
            now + super::super::health::HEARTBEAT_INTERVAL
                + super::super::health::HEARTBEAT_TIMEOUT,
        );
        assert!(registry.connection(&ssh_id).is_none());
        assert_eq!(registry.take_failures()[0].kind, io::ErrorKind::TimedOut);
    }

    /// The Windows named pipe measured in herdr-8qd: the writer offers a large frame in
    /// 512-byte pieces and a peer polling through PeekNamedPipe takes one piece per poll. A
    /// 1 MiB paste is about 2048 pieces, roughly 20 s, well past the 15 s the health check
    /// allows between received frames.
    const DRAIN_PIECE: usize = 512;
    const DRAIN_PIECE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

    /// A writer driven by the test's own clock: `accept_until` moves the same virtual time the
    /// registry is ticked with, so a 20 s drain costs no wall-clock time.
    struct DrainingWriter {
        queued: std::collections::VecDeque<(usize, Option<TransmitReceipt>)>,
        accepted_through: Instant,
        stalled: bool,
        transmitted: Vec<Instant>,
        error: Option<io::Error>,
    }

    impl DrainingWriter {
        fn new(now: Instant, stalled: bool) -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(Self {
                queued: std::collections::VecDeque::new(),
                accepted_through: now,
                stalled,
                transmitted: Vec::new(),
                error: None,
            }))
        }

        fn accept_until(&mut self, now: Instant) {
            while self.accepted_through + DRAIN_PIECE_INTERVAL <= now {
                if self.stalled {
                    return;
                }
                self.accepted_through += DRAIN_PIECE_INTERVAL;
                let Some(frame) = self.queued.front_mut() else {
                    continue;
                };
                frame.0 = frame.0.saturating_sub(DRAIN_PIECE);
                if frame.0 > 0 {
                    continue;
                }
                let accepted_at = self.accepted_through;
                if let Some((_, Some(receipt))) = self.queued.pop_front() {
                    receipt.complete(accepted_at);
                    self.transmitted.push(accepted_at);
                }
            }
        }
    }

    struct DrainingTransport(Arc<Mutex<DrainingWriter>>);

    impl DrainingTransport {
        fn queue(&self, message: &ClientMessage, receipt: Option<TransmitReceipt>) {
            let mut frame = Vec::new();
            crate::protocol::write_message(&mut frame, message).unwrap();
            self.0
                .lock()
                .unwrap()
                .queued
                .push_back((frame.len(), receipt));
        }
    }

    impl EndpointTransport for DrainingTransport {
        fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
            self.queue(message, None);
            Ok(())
        }

        fn send_tracked(
            &mut self,
            message: &ClientMessage,
            _sent_at: Instant,
        ) -> io::Result<TransmitReceipt> {
            let receipt = TransmitReceipt::pending();
            self.queue(message, Some(receipt.clone()));
            Ok(receipt)
        }

        fn take_error(&mut self) -> Option<io::Error> {
            self.0.lock().unwrap().error.take()
        }
    }

    fn draining_registry(
        now: Instant,
        stalled: bool,
    ) -> (
        EndpointRegistry,
        ClientEndpointId,
        Arc<Mutex<DrainingWriter>>,
    ) {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            1,
            negotiation(),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        let writer = DrainingWriter::new(now, stalled);
        registry.insert(
            ssh_id.clone(),
            DrainingTransport(writer.clone()),
            2,
            negotiation(),
            true,
        );
        registry.mark_ready(&ssh_id, 2);
        registry.received(&ssh_id, 2, now);
        (registry, ssh_id, writer)
    }

    #[test]
    fn a_paste_that_is_still_draining_does_not_expire_a_quiet_endpoint() {
        // bd herdr-7ak, from the codex review of herdr-8qd: a 1 MiB paste to a pane that prints
        // nothing drains in 512-byte pieces for about 20 s. The health ping is queued behind it
        // at 5 s and, with the probe timer started at enqueue, the connection was cut at 15 s
        // while its own frame was still moving.
        let now = Instant::now();
        let (mut registry, ssh_id, writer) = draining_registry(now, false);
        assert_eq!(
            registry.send_to(
                &ssh_id,
                &ClientMessage::Input {
                    data: vec![b'p'; 1024 * 1024],
                }
            ),
            EndpointSendOutcome::Sent
        );

        let mut at = now;
        while at < now + std::time::Duration::from_secs(25) {
            at += std::time::Duration::from_millis(250);
            writer.lock().unwrap().accept_until(at);
            registry.tick_health(at);
            assert!(
                registry.connection(&ssh_id).is_some(),
                "the health check cut the endpoint {:?} into a paste that was still draining",
                at.saturating_duration_since(now)
            );
            assert!(registry.take_failures().is_empty());
        }

        let transmitted = writer.lock().unwrap().transmitted.clone();
        assert_eq!(
            transmitted.len(),
            1,
            "the ping is the one frame whose transmission the registry tracks"
        );
        let ping_at = transmitted[0];
        assert!(
            ping_at
                > now + super::super::health::HEARTBEAT_INTERVAL
                    + super::super::health::HEARTBEAT_TIMEOUT,
            "the ping left the writer inside the old 15 s window, so this test does not cover the bug"
        );
        assert!(!registry
            .connection(&ssh_id)
            .unwrap()
            .health
            .as_ref()
            .unwrap()
            .is_draining());

        registry.tick_health(
            ping_at + super::super::health::HEARTBEAT_TIMEOUT - std::time::Duration::from_millis(1),
        );
        assert!(
            registry.connection(&ssh_id).is_some(),
            "the reply window must run from the instant the writer took the ping"
        );
        registry.tick_health(ping_at + super::super::health::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_none());
        assert_eq!(registry.take_failures()[0].kind, io::ErrorKind::TimedOut);
    }

    #[test]
    fn a_stalled_writer_is_still_cut_by_its_own_stall_timeout() {
        // herdr-8qd's stall timeout stays the cut for a frame that stops moving: the health
        // check leaves a queued ping alone, and the writer's timeout reaches the registry as a
        // transport failure instead.
        let now = Instant::now();
        let (mut registry, ssh_id, writer) = draining_registry(now, true);
        assert_eq!(
            registry.send_to(
                &ssh_id,
                &ClientMessage::Input {
                    data: vec![b'p'; 1024 * 1024],
                }
            ),
            EndpointSendOutcome::Sent
        );

        let mut at = now;
        while at < now + std::time::Duration::from_secs(30) {
            at += std::time::Duration::from_millis(250);
            writer.lock().unwrap().accept_until(at);
            registry.tick_health(at);
        }
        assert!(
            registry.connection(&ssh_id).is_some(),
            "the health check must not double-cut a stall the writer owns"
        );
        assert!(registry
            .connection(&ssh_id)
            .unwrap()
            .health
            .as_ref()
            .unwrap()
            .is_draining());
        assert!(writer.lock().unwrap().transmitted.is_empty());

        writer.lock().unwrap().error = Some(io::Error::new(
            io::ErrorKind::TimedOut,
            "endpoint write timed out",
        ));
        let failures = registry.take_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].kind, io::ErrorKind::TimedOut);
        assert!(registry.connection(&ssh_id).is_none());
    }

    #[test]
    fn a_ready_endpoint_can_stay_connected_after_the_initial_deadline() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            1,
            negotiation(),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            2,
            negotiation(),
            false,
        );
        let now = Instant::now();
        registry.mark_ready(&ssh_id, 2);
        registry.received(&ssh_id, 2, now + super::super::health::HEARTBEAT_INTERVAL);
        registry.tick_health(now + super::super::health::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_some());
    }

    #[test]
    fn negotiated_surface_interest_requires_capability_and_method() {
        assert!(negotiation().supports_surface_interest());
        assert!(negotiation().supports_health_check());
        assert!(
            !EndpointNegotiation::new(vec!["client_shell.surface.set".into()], Vec::new())
                .supports_surface_interest()
        );
        assert!(!EndpointNegotiation::new(
            Vec::new(),
            vec![crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY.into()]
        )
        .supports_surface_interest());
        assert!(!EndpointNegotiation::new(
            vec!["client_shell.surface.set".into()],
            vec![crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY.into()]
        )
        .supports_surface_interest());
    }

    #[test]
    fn dropping_registry_detaches_every_connected_endpoint() {
        let local_sent = Arc::new(Mutex::new(Vec::new()));
        let remote_sent = Arc::new(Mutex::new(Vec::new()));
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: local_sent.clone(),
                error: None,
            },
            1,
            negotiation(),
        );
        registry.insert(
            ClientEndpointId::Ssh(profile()),
            FakeTransport {
                sent: remote_sent.clone(),
                error: None,
            },
            2,
            negotiation(),
            false,
        );

        drop(registry);

        assert!(matches!(
            local_sent.lock().unwrap().as_slice(),
            [ClientMessage::Detach]
        ));
        assert!(matches!(
            remote_sent.lock().unwrap().as_slice(),
            [ClientMessage::Detach]
        ));
    }

    #[test]
    fn stale_generations_are_rejected() {
        let registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            7,
            negotiation(),
        );
        assert!(registry.accepts(&ClientEndpointId::Local, 7));
        assert!(!registry.accepts(&ClientEndpointId::Local, 6));
    }
}
