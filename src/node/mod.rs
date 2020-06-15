// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod stage;
#[cfg(all(test, feature = "mock"))]
mod tests;

#[cfg(feature = "mock_base")]
pub use self::stage::{BOOTSTRAP_TIMEOUT, JOIN_TIMEOUT};

use self::stage::{Approved, Bootstrapping, JoinParams, Joining, RelocateParams, Stage};
use crate::{
    consensus::GenesisPrefixInfo,
    core::Core,
    error::{Result, RoutingError},
    event::{Connected, Event},
    id::{FullId, P2pNode, PublicId},
    location::{DstLocation, SrcLocation},
    log_utils,
    messages::{
        BootstrapResponse, Message, MessageHash, MessageStatus, MessageWithBytes, QueuedMessage,
        Variant,
    },
    network_params::NetworkParams,
    pause::PausedState,
    quic_p2p::{EventSenders, Peer, Token},
    relocation::SignedRelocateDetails,
    rng::{self, MainRng},
    transport::PeerStatus,
    xor_space::{Prefix, XorName, Xorable},
    TransportConfig, TransportEvent,
};
use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvError, Select};
use itertools::Itertools;
use std::net::SocketAddr;

#[cfg(all(test, feature = "mock"))]
use crate::{consensus::ConsensusEngine, messages::AccumulatingMessage};
#[cfg(feature = "mock_base")]
use {
    crate::section::{EldersInfo, SectionProofChain, SharedState},
    std::collections::{BTreeMap, BTreeSet},
};

/// Node configuration.
pub struct NodeConfig {
    /// If true, configures the node to start a new network instead of joining an existing one.
    pub first: bool,
    /// The ID of the node or `None` for randomly generated one.
    pub full_id: Option<FullId>,
    /// Configuration for the underlying network transport.
    pub transport_config: TransportConfig,
    /// Global network parameters. Must be identical for all nodes in the network.
    pub network_params: NetworkParams,
    /// Random number generator to be used by the node. Can be used to achieve repeatable tests by
    /// providing a pre-seeded RNG. By default uses a random seed provided by the OS.
    pub rng: MainRng,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            first: false,
            full_id: None,
            transport_config: TransportConfig::default(),
            network_params: NetworkParams::default(),
            rng: rng::new(),
        }
    }
}

/// Interface for sending and receiving messages to and from other nodes, in the role of a full
/// routing node.
///
/// A node is a part of the network that can route messages and be a member of a section or group
/// location. Its methods can be used to send requests and responses as either an individual
/// `Node` or as a part of a section or group location. Their `src` argument indicates that
/// role, and can be any [`SrcLocation`](enum.SrcLocation.html).
pub struct Node {
    core: Core,
    stage: Stage,

    timer_rx: Receiver<u64>,
    timer_rx_idx: usize,
    transport_rx: Receiver<TransportEvent>,
    transport_rx_idx: usize,
}

impl Node {
    ////////////////////////////////////////////////////////////////////////////
    // Public API
    ////////////////////////////////////////////////////////////////////////////

    /// Create new node using the given config.
    ///
    /// Returns the node itself, the user event receiver and the client network
    /// event receiver.
    pub fn new(config: NodeConfig) -> (Self, Receiver<Event>, Receiver<TransportEvent>) {
        let (timer_tx, timer_rx) = crossbeam_channel::unbounded();
        let (transport_tx, transport_node_rx, transport_client_rx) = transport_channels();
        let (user_event_tx, user_event_rx) = crossbeam_channel::unbounded();

        let first = config.first;
        let mut core = Core::new(config, timer_tx, transport_tx, user_event_tx);

        let stage = if first {
            match Approved::first(&mut core) {
                Ok(stage) => {
                    info!("{} Started a new network as a seed node.", core.name());
                    core.send_event(Event::Connected(Connected::First));
                    core.send_event(Event::Promoted);
                    Stage::Approved(stage)
                }
                Err(error) => {
                    error!(
                        "{} Failed to start the first node: {:?}",
                        core.name(),
                        error
                    );
                    Stage::Terminated
                }
            }
        } else {
            info!("{} Bootstrapping a new node.", core.name());
            core.transport.bootstrap();
            Stage::Bootstrapping(Bootstrapping::new(None))
        };

        let node = Self {
            stage,
            core,
            timer_rx,
            timer_rx_idx: 0,
            transport_rx: transport_node_rx,
            transport_rx_idx: 0,
        };

        (node, user_event_rx, transport_client_rx)
    }

    /// Pauses the node in order to be upgraded and/or restarted.
    /// Returns `InvalidState` error if the node is not a member of any section yet.
    pub fn pause(self) -> Result<PausedState> {
        if let Stage::Approved(stage) = self.stage {
            info!("Pause");

            let mut state = stage.pause(self.core);
            state.transport_rx = Some(self.transport_rx);

            Ok(state)
        } else {
            Err(RoutingError::InvalidState)
        }
    }

    /// Resume previously paused node.
    pub fn resume(mut state: PausedState) -> (Self, Receiver<Event>) {
        let (timer_tx, timer_rx) = crossbeam_channel::unbounded();
        let transport_rx = state
            .transport_rx
            .take()
            .expect("PausedState is incomplete");
        let (user_event_tx, user_event_rx) = crossbeam_channel::unbounded();

        let (stage, core) = Approved::resume(state, timer_tx, user_event_tx);

        info!("Resume");

        let node = Self {
            stage: Stage::Approved(stage),
            core,
            timer_rx,
            timer_rx_idx: 0,
            transport_rx,
            transport_rx_idx: 0,
        };

        (node, user_event_rx)
    }

    /// Register the node event channels with the provided [selector](mpmc::Select).
    pub fn register<'a>(&'a mut self, select: &mut Select<'a>) {
        // Populate action_rx timeouts
        #[cfg(feature = "mock_base")]
        self.core.timer.process_timers();

        self.timer_rx_idx = select.recv(&self.timer_rx);
        self.transport_rx_idx = select.recv(&self.transport_rx);
    }

    /// Processes events received externally from one of the channels.
    /// For this function to work properly, the node event channels need to
    /// be registered by calling [`ApprovedPeer::register`](#method.register).
    /// [`Select::ready`] needs to be called to get `op_index`, the event channel index.
    ///
    /// This function is non-blocking.
    ///
    /// Errors are permanent failures due to either: node termination, the permanent closing of one
    /// of the event channels, or an invalid (unknown) channel index.
    ///
    /// [`Select::ready`]: https://docs.rs/crossbeam-channel/0.3/crossbeam_channel/struct.Select.html#method.ready
    pub fn handle_selected_operation(&mut self, op_index: usize) -> Result<(), RecvError> {
        if !self.is_running() {
            return Err(RecvError);
        }

        let _log_ident = self.set_log_ident();
        match op_index {
            idx if idx == self.transport_rx_idx => {
                let event = self.transport_rx.recv()?;
                self.handle_transport_event(event);
            }
            idx if idx == self.timer_rx_idx => {
                let token = self.timer_rx.recv()?;
                self.handle_timeout(token);
            }
            _idx => return Err(RecvError),
        };

        self.handle_messages();

        if let Stage::Approved(stage) = &mut self.stage {
            stage.finish_handle_input(&mut self.core);
        }

        Ok(())
    }

    /// Returns whether this node is running or has been terminated.
    pub fn is_running(&self) -> bool {
        !matches!(self.stage, Stage::Terminated)
    }

    /// Returns the `PublicId` of this node.
    pub fn id(&self) -> &PublicId {
        self.core.id()
    }

    /// The name of this node.
    pub fn name(&self) -> &XorName {
        self.id().name()
    }

    /// Returns connection info of this node.
    pub fn our_connection_info(&mut self) -> Result<SocketAddr> {
        self.core.our_connection_info()
    }

    /// Our `Prefix` once we are a part of the section.
    pub fn our_prefix(&self) -> Option<&Prefix<XorName>> {
        if let Stage::Approved(stage) = &self.stage {
            Some(stage.shared_state.our_prefix())
        } else {
            None
        }
    }

    /// Finds out if the given XorName matches our prefix. Returns error if we don't have a prefix
    /// because we haven't joined any section yet.
    pub fn matches_our_prefix(&self, name: &XorName) -> Result<bool> {
        if let Some(prefix) = self.our_prefix() {
            Ok(prefix.matches(name))
        } else {
            Err(RoutingError::InvalidState)
        }
    }

    /// Returns whether the node is Elder.
    pub fn is_elder(&self) -> bool {
        self.stage
            .approved()
            .map(|stage| {
                stage
                    .shared_state
                    .sections
                    .our()
                    .elders
                    .contains_key(self.core.name())
            })
            .unwrap_or(false)
    }

    /// Returns the information of all the current section elders.
    pub fn our_elders(&self) -> impl Iterator<Item = &P2pNode> {
        self.stage
            .approved()
            .into_iter()
            .flat_map(|stage| stage.shared_state.sections.our_elders())
    }

    /// Returns the elders of our section sorted by their distance to `name` (closest first).
    pub fn our_elders_sorted_by_distance_to(&self, name: &XorName) -> Vec<&P2pNode> {
        self.our_elders()
            .sorted_by(|lhs, rhs| name.cmp_distance(lhs.name(), rhs.name()))
    }

    /// Returns the information of all the current section adults.
    pub fn our_adults(&self) -> impl Iterator<Item = &P2pNode> {
        self.stage
            .approved()
            .into_iter()
            .flat_map(|stage| stage.shared_state.our_adults())
    }

    /// Returns the adults of our section sorted by their distance to `name` (closest first).
    /// If we are not elder or if there are no adults in the section, returns empty vec.
    pub fn our_adults_sorted_by_distance_to(&self, name: &XorName) -> Vec<&P2pNode> {
        self.our_adults()
            .sorted_by(|lhs, rhs| name.cmp_distance(lhs.name(), rhs.name()))
    }

    /// Checks whether the given location represents self.
    pub fn in_dst_location(&self, dst: &DstLocation) -> bool {
        match &self.stage {
            Stage::Bootstrapping(_) | Stage::Joining(_) => match dst {
                DstLocation::Node(name) => name == self.core.name(),
                DstLocation::Section(_) => false,
                DstLocation::Direct => true,
            },
            Stage::Approved(stage) => {
                dst.contains(self.core.name(), stage.shared_state.our_prefix())
            }
            Stage::Terminated => false,
        }
    }

    /// Vote for a user-defined event.
    /// Returns `InvalidState` error if we are not an elder.
    pub fn vote_for_user_event(&mut self, event: Vec<u8>) -> Result<()> {
        let our_id = self.core.id();
        if let Some(stage) = self
            .stage
            .approved_mut()
            .filter(|stage| stage.is_our_elder(our_id))
        {
            stage.vote_for_user_event(event);
            Ok(())
        } else {
            Err(RoutingError::InvalidState)
        }
    }

    /// Send a message.
    pub fn send_message(
        &mut self,
        src: SrcLocation,
        dst: DstLocation,
        content: Vec<u8>,
    ) -> Result<(), RoutingError> {
        if let DstLocation::Direct = dst {
            return Err(RoutingError::BadLocation);
        }

        let _log_ident = self.set_log_ident();

        match &mut self.stage {
            Stage::Bootstrapping(_) | Stage::Joining(_) | Stage::Terminated => {
                Err(RoutingError::InvalidState)
            }
            Stage::Approved(stage) => stage.send_routing_message(
                &mut self.core,
                src,
                dst,
                Variant::UserMessage(content),
                None,
            ),
        }
    }

    /// Send a message to a client peer.
    pub fn send_message_to_client(
        &mut self,
        peer_addr: SocketAddr,
        msg: Bytes,
        token: Token,
    ) -> Result<()> {
        self.core
            .transport
            .send_message_to_client(peer_addr, msg, token);
        Ok(())
    }

    /// Disconnect form a client peer.
    pub fn disconnect_from_client(&mut self, peer_addr: SocketAddr) -> Result<()> {
        self.core.transport.disconnect(peer_addr);
        Ok(())
    }

    /// Returns the current BLS public key set or `RoutingError::InvalidState` if we are not joined
    /// yet.
    pub fn public_key_set(&self) -> Result<&bls::PublicKeySet> {
        self.stage
            .approved()
            .map(|stage| stage.public_key_set())
            .ok_or(RoutingError::InvalidState)
    }

    /// Returns the current BLS secret key share or `RoutingError::InvalidState` if we are not
    /// elder.
    pub fn secret_key_share(&self) -> Result<&bls::SecretKeyShare> {
        self.stage
            .approved()
            .and_then(|stage| stage.secret_key_share())
            .ok_or(RoutingError::InvalidState)
    }

    ////////////////////////////////////////////////////////////////////////////
    // Input handling
    ////////////////////////////////////////////////////////////////////////////

    fn handle_transport_event(&mut self, event: TransportEvent) {
        use crate::TransportEvent::*;

        match event {
            BootstrappedTo { node } => self.handle_bootstrapped_to(node),
            BootstrapFailure => self.handle_bootstrap_failure(),
            ConnectedTo { .. } => (),
            ConnectionFailure { peer, .. } => match peer {
                Peer::Client(_) => (),
                Peer::Node(peer_addr) => self.handle_connection_failure(peer_addr),
            },
            NewMessage { peer, msg } => match peer {
                Peer::Client(_) => (),
                Peer::Node(peer_addr) => self.handle_new_message(peer_addr, msg),
            },
            UnsentUserMessage { peer, msg, token } => match peer {
                Peer::Client(_) => (),
                Peer::Node(peer_addr) => self.handle_unsent_message(peer_addr, msg, token),
            },
            SentUserMessage { peer, msg, token } => match peer {
                Peer::Client(_) => (),
                Peer::Node(peer_addr) => self.handle_sent_message(peer_addr, msg, token),
            },
            Finish => {
                self.stage = Stage::Terminated;
            }
        }
    }

    fn handle_bootstrapped_to(&mut self, addr: SocketAddr) {
        match &mut self.stage {
            Stage::Bootstrapping(stage) => stage.send_bootstrap_request(&mut self.core, addr),
            Stage::Joining(_) | Stage::Approved(_) => {
                // A bootstrapped node doesn't need another bootstrap connection
                self.core.transport.disconnect(addr);
            }
            Stage::Terminated => {}
        }
    }

    fn handle_bootstrap_failure(&mut self) {
        assert!(matches!(self.stage, Stage::Bootstrapping(_)));

        info!("Failed to bootstrap. Terminating.");
        self.core.send_event(Event::Terminated);
        self.stage = Stage::Terminated;
    }

    fn handle_connection_failure(&mut self, addr: SocketAddr) {
        if let Stage::Approved(stage) = &mut self.stage {
            stage.handle_connection_failure(&mut self.core, addr);
        } else {
            trace!("ConnectionFailure from {}", addr);
        }
    }

    fn handle_new_message(&mut self, sender: SocketAddr, bytes: Bytes) {
        let msg = match MessageWithBytes::partial_from_bytes(bytes) {
            Ok(msg) => msg,
            Err(error) => {
                debug!("Failed to deserialize message: {:?}", error);
                return;
            }
        };

        if let Err(error) = self.try_handle_message(sender, msg) {
            debug!("Failed to handle message: {:?}", error);
        }
    }

    fn handle_unsent_message(&mut self, addr: SocketAddr, msg: Bytes, msg_token: Token) {
        match self.core.handle_unsent_message(addr, msg, msg_token) {
            PeerStatus::Normal => (),
            PeerStatus::Lost => self.handle_peer_lost(addr),
        }
    }

    fn handle_sent_message(&mut self, addr: SocketAddr, _msg: Bytes, token: Token) {
        trace!("Successfully sent message with ID {} to {:?}", token, addr);
        self.core.transport.target_succeeded(token, addr);
    }

    fn handle_timeout(&mut self, token: u64) {
        if self.core.transport.handle_timeout(token) {
            return;
        }

        match &mut self.stage {
            Stage::Bootstrapping(stage) => stage.handle_timeout(&mut self.core, token),
            Stage::Joining(stage) => stage.handle_timeout(&mut self.core, token),
            Stage::Approved(stage) => stage.handle_timeout(&mut self.core, token),
            Stage::Terminated => {}
        }
    }

    fn handle_peer_lost(&mut self, peer_addr: SocketAddr) {
        if let Stage::Approved(stage) = &mut self.stage {
            stage.handle_peer_lost(&self.core, peer_addr);
        }
    }

    ////////////////////////////////////////////////////////////////////////////
    // Message handling
    ////////////////////////////////////////////////////////////////////////////

    fn try_handle_message(
        &mut self,
        sender: SocketAddr,
        mut msg_with_bytes: MessageWithBytes,
    ) -> Result<()> {
        trace!("try handle message {:?}", msg_with_bytes);

        self.try_relay_message(&msg_with_bytes)?;

        if !self.in_dst_location(msg_with_bytes.message_dst()) {
            return Ok(());
        }

        if self.core.msg_filter.contains_incoming(&msg_with_bytes) {
            trace!(
                "not handling message - already handled: {:?}",
                msg_with_bytes
            );
            return Ok(());
        }

        let msg = msg_with_bytes.take_or_deserialize_message()?;

        match self.decide_message_status(&msg)? {
            MessageStatus::Useful => {
                self.core.msg_filter.insert_incoming(&msg_with_bytes);
                self.handle_message(sender, msg, msg_with_bytes.full_crypto_hash())
            }
            MessageStatus::Untrusted => {
                debug!(
                    "Untrusted message from {}: {:?} (hash: {:?})",
                    sender,
                    msg,
                    msg_with_bytes.full_crypto_hash()
                );
                self.handle_untrusted_message(sender, msg)
            }
            MessageStatus::Unknown => {
                debug!(
                    "Unknown message from {}: {:?} (hash: {:?})",
                    sender,
                    msg,
                    msg_with_bytes.full_crypto_hash(),
                );
                self.handle_unknown_message(sender, msg, msg_with_bytes.full_bytes().clone())
            }
            MessageStatus::Useless => {
                debug!("Useless message from {}: {:?}", sender, msg);
                Ok(())
            }
        }
    }

    fn try_relay_message(&mut self, msg: &MessageWithBytes) -> Result<()> {
        if !self.in_dst_location(msg.message_dst()) || msg.message_dst().is_section() {
            // Relay closer to the destination or broadcast to the rest of our section.
            self.relay_message(msg)
        } else {
            Ok(())
        }
    }

    fn relay_message(&mut self, msg: &MessageWithBytes) -> Result<()> {
        match &mut self.stage {
            Stage::Approved(stage) => stage.relay_message(&mut self.core, msg),
            Stage::Bootstrapping(_) | Stage::Joining(_) | Stage::Terminated => Ok(()),
        }
    }

    fn decide_message_status(&self, msg: &Message) -> Result<MessageStatus> {
        match &self.stage {
            Stage::Bootstrapping(stage) => stage.decide_message_status(msg),
            Stage::Joining(stage) => stage.decide_message_status(msg),
            Stage::Approved(stage) => stage.decide_message_status(self.core.id(), msg),
            Stage::Terminated => Ok(MessageStatus::Useless),
        }
    }

    fn handle_message(
        &mut self,
        sender: SocketAddr,
        msg: Message,
        msg_hash: &MessageHash,
    ) -> Result<()> {
        if let Stage::Approved(stage) = &mut self.stage {
            stage.update_section_knowledge(&msg, msg_hash);
        }

        self.core.msg_queue.push_back(msg.into_queued(Some(sender)));

        Ok(())
    }

    fn handle_messages(&mut self) {
        while let Some(QueuedMessage { message, sender }) = self.core.msg_queue.pop_front() {
            if self.in_dst_location(&message.dst) {
                match self.dispatch_message(sender, message) {
                    Ok(()) => (),
                    Err(err) => debug!("Routing message dispatch failed: {:?}", err),
                }
            }
        }
    }

    fn dispatch_message(&mut self, sender: Option<SocketAddr>, msg: Message) -> Result<()> {
        trace!("Got {:?}", msg);

        match &mut self.stage {
            Stage::Bootstrapping(stage) => match msg.variant {
                Variant::BootstrapResponse(response) => {
                    if let Some(params) = stage.handle_bootstrap_response(
                        &mut self.core,
                        msg.src.to_sender_node(sender)?,
                        response,
                    )? {
                        self.join(params);
                    }
                }
                _ => unreachable!(),
            },
            Stage::Joining(stage) => match msg.variant {
                Variant::BootstrapResponse(BootstrapResponse::Join {
                    elders_info,
                    section_key,
                }) => stage.handle_bootstrap_response(
                    &mut self.core,
                    msg.src.to_sender_node(sender)?,
                    elders_info,
                    section_key,
                )?,
                Variant::NodeApproval(genesis_prefix_info) => {
                    let connect_type = stage.connect_type();
                    let msg_backlog = stage.take_message_backlog();
                    self.approve(connect_type, genesis_prefix_info, msg_backlog)?
                }
                _ => unreachable!(),
            },
            Stage::Approved(stage) => match msg.variant {
                Variant::NeighbourInfo { elders_info, .. } => {
                    msg.dst.check_is_section()?;
                    let src_key = *msg.src.as_section_key()?;
                    stage.handle_neighbour_info(elders_info, src_key)?;
                }
                Variant::GenesisUpdate(info) => {
                    msg.src.check_is_section()?;
                    stage.handle_genesis_update(&mut self.core, info)?;
                }
                Variant::Relocate(_) => {
                    msg.src.check_is_section()?;
                    let signed_relocate = SignedRelocateDetails::new(msg)?;
                    if let Some(params) = stage.handle_relocate(&mut self.core, signed_relocate) {
                        self.relocate(params)
                    }
                }
                Variant::MessageSignature(accumulating_msg) => {
                    stage.handle_message_signature(
                        &mut self.core,
                        *accumulating_msg,
                        *msg.src.as_node()?,
                    )?;
                }
                Variant::BootstrapRequest(name) => stage.handle_bootstrap_request(
                    &mut self.core,
                    msg.src.to_sender_node(sender)?,
                    name,
                ),
                Variant::JoinRequest(join_request) => stage.handle_join_request(
                    &mut self.core,
                    msg.src.to_sender_node(sender)?,
                    *join_request,
                ),
                Variant::ParsecPoke(version) => stage.handle_parsec_poke(
                    &mut self.core,
                    msg.src.to_sender_node(sender)?,
                    version,
                ),
                Variant::ParsecRequest(version, request) => {
                    stage.handle_parsec_request(
                        &mut self.core,
                        version,
                        request,
                        msg.src.to_sender_node(sender)?,
                    )?;
                }
                Variant::ParsecResponse(version, response) => {
                    stage.handle_parsec_response(
                        &mut self.core,
                        version,
                        response,
                        *msg.src.as_node()?,
                    )?;
                }
                Variant::UserMessage(content) => {
                    self.core.send_event(Event::MessageReceived {
                        content,
                        src: msg.src.location(),
                        dst: msg.dst,
                    });
                }
                Variant::BouncedUntrustedMessage(message) => {
                    stage.handle_bounced_untrusted_message(&mut self.core, msg.dst_key, *message)?
                }
                Variant::BouncedUnknownMessage {
                    message,
                    parsec_version,
                } => stage.handle_bounced_unknown_message(
                    &mut self.core,
                    msg.src.to_sender_node(sender)?,
                    message,
                    parsec_version,
                ),
                Variant::NodeApproval(_) | Variant::BootstrapResponse(_) | Variant::Ping => {
                    unreachable!()
                }
            },
            Stage::Terminated => unreachable!(),
        }

        Ok(())
    }

    fn handle_untrusted_message(&mut self, sender: SocketAddr, msg: Message) -> Result<()> {
        match &self.stage {
            Stage::Approved(stage) => {
                stage.handle_untrusted_message(&mut self.core, Some(sender), msg)
            }
            Stage::Bootstrapping(_) | Stage::Joining(_) | Stage::Terminated => unreachable!(),
        }
    }

    fn handle_unknown_message(
        &mut self,
        sender: SocketAddr,
        msg: Message,
        msg_bytes: Bytes,
    ) -> Result<()> {
        match &mut self.stage {
            Stage::Bootstrapping(stage) => stage.handle_unknown_message(sender, msg),
            Stage::Joining(stage) => stage.handle_unknown_message(sender, msg),
            Stage::Approved(stage) => {
                stage.handle_unknown_message(&mut self.core, Some(sender), msg_bytes)?
            }
            Stage::Terminated => (),
        }

        Ok(())
    }

    ////////////////////////////////////////////////////////////////////////////
    // Transitions
    ////////////////////////////////////////////////////////////////////////////

    // Transition from Bootstrapping to Joining
    fn join(&mut self, params: JoinParams) {
        let JoinParams {
            elders_info,
            section_key,
            relocate_payload,
            msg_backlog,
        } = params;

        self.stage = Stage::Joining(Joining::new(
            &mut self.core,
            elders_info,
            section_key,
            relocate_payload,
            msg_backlog,
        ));
    }

    // Transition from Joining to Approved
    fn approve(
        &mut self,
        connect_type: Connected,
        genesis_prefix_info: GenesisPrefixInfo,
        msg_backlog: Vec<QueuedMessage>,
    ) -> Result<()> {
        info!(
            "This node has been approved to join the network at {:?}!",
            genesis_prefix_info.elders_info.prefix,
        );

        let stage = Approved::new(&mut self.core, genesis_prefix_info, None)?;
        self.stage = Stage::Approved(stage);

        self.core.msg_queue.extend(msg_backlog);
        self.core.send_event(Event::Connected(connect_type));

        Ok(())
    }

    // Transition from Approved to Bootstrapping on relocation
    fn relocate(&mut self, params: RelocateParams) {
        let RelocateParams {
            conn_infos,
            details,
        } = params;

        let mut stage = Bootstrapping::new(Some(details));

        for conn_info in conn_infos {
            stage.send_bootstrap_request(&mut self.core, conn_info)
        }

        self.stage = Stage::Bootstrapping(stage);
    }

    fn set_log_ident(&self) -> log_utils::Guard {
        use std::fmt::Write;
        log_utils::set_ident(|buffer| match &self.stage {
            Stage::Bootstrapping(_) => write!(buffer, "{}(?) ", self.name()),
            Stage::Joining(stage) => write!(
                buffer,
                "{}({:b}?) ",
                self.name(),
                stage.target_section_elders_info().prefix,
            ),
            Stage::Approved(stage) => {
                if stage.is_our_elder(self.core.id()) {
                    write!(
                        buffer,
                        "{}({:b}v{}!) ",
                        self.core.name(),
                        stage.shared_state.our_prefix(),
                        stage.shared_state.our_history.last_key_index()
                    )
                } else {
                    write!(
                        buffer,
                        "{}({:b}) ",
                        self.core.name(),
                        stage.shared_state.our_prefix()
                    )
                }
            }
            Stage::Terminated => write!(buffer, "[terminated]"),
        })
    }
}

#[cfg(feature = "mock_base")]
impl Node {
    /// Returns whether the node is approved member of a section.
    pub fn is_approved(&self) -> bool {
        self.stage.approved().is_some()
    }

    /// Indicates if there are any pending observations in the parsec object
    pub fn has_unpolled_observations(&self) -> bool {
        self.stage
            .approved()
            .map(|stage| {
                stage
                    .consensus_engine
                    .parsec_map()
                    .has_unpolled_observations()
            })
            .unwrap_or(false)
    }

    /// Send a message to the given targets using the given delivery group size.
    pub fn send_message_to_targets(
        &mut self,
        dst_targets: &[SocketAddr],
        delivery_group_size: usize,
        message: Message,
    ) -> Result<(), RoutingError> {
        let message = message.to_bytes()?;
        self.core
            .send_message_to_targets(dst_targets, delivery_group_size, message);
        Ok(())
    }

    /// Returns the version of the latest Parsec instance of this node.
    pub fn parsec_last_version(&self) -> u64 {
        self.stage
            .approved()
            .map(|stage| stage.consensus_engine.parsec_version())
            .unwrap_or(0)
    }

    /// Checks whether the given location represents self.
    pub fn in_src_location(&self, src: &SrcLocation) -> bool {
        src.contains(self.core.name())
    }

    /// Returns the info about our neighbour sections.
    pub fn neighbour_sections(&self) -> impl Iterator<Item = &EldersInfo> {
        self.shared_state()
            .into_iter()
            .flat_map(|state| state.sections.other().map(|(_, info)| info))
    }

    /// Returns the info about our sections or `None` if we are not joined yet.
    pub fn our_section(&self) -> Option<&EldersInfo> {
        self.shared_state().map(|state| state.sections.our())
    }

    /// Returns the prefixes of all sections known to us
    pub fn prefixes(&self) -> BTreeSet<Prefix<XorName>> {
        self.shared_state()
            .map(|state| state.sections.prefixes().copied().collect())
            .unwrap_or_default()
    }

    /// Returns the elders in our and neighbouring sections.
    pub fn known_elders(&self) -> impl Iterator<Item = &P2pNode> {
        self.shared_state()
            .into_iter()
            .flat_map(|state| state.sections.elders())
    }

    /// Returns whether the given peer is an elder known to us.
    pub fn is_peer_elder(&self, name: &XorName) -> bool {
        self.shared_state()
            .map(|state| state.is_peer_elder(name))
            .unwrap_or(false)
    }

    /// Returns whether the given peer is an elder of our section.
    pub fn is_peer_our_elder(&self, name: &XorName) -> bool {
        self.shared_state()
            .map(|state| state.is_peer_our_elder(name))
            .unwrap_or(false)
    }

    /// Returns the members in our section and elders we know.
    pub fn known_nodes(&self) -> impl Iterator<Item = &P2pNode> {
        self.shared_state()
            .into_iter()
            .flat_map(|state| state.known_nodes())
    }

    /// Returns whether the given `XorName` is a member of our section.
    pub fn is_peer_our_member(&self, name: &XorName) -> bool {
        self.shared_state()
            .map(|state| state.our_members.contains(name))
            .unwrap_or(false)
    }

    /// Returns their knowledge
    pub fn get_their_knowledge(&self) -> BTreeMap<Prefix<XorName>, u64> {
        self.shared_state()
            .map(|state| state.sections.knowledge())
            .cloned()
            .unwrap_or_default()
    }

    /// If our section is the closest one to `name`, returns all names in our section *including
    /// ours*, otherwise returns `None`.
    pub fn close_names(&self, name: &XorName) -> Option<Vec<XorName>> {
        let state = self.shared_state()?;
        if state.our_prefix().matches(name) {
            Some(
                state
                    .sections
                    .our_elders()
                    .map(|p2p_node| *p2p_node.name())
                    .collect(),
            )
        } else {
            None
        }
    }

    /// Returns the number of elders this node is using.
    pub fn elder_size(&self) -> usize {
        self.core.network_params.elder_size
    }

    /// Size at which our section splits. Since this is configurable, this method is used to
    /// obtain it.
    pub fn recommended_section_size(&self) -> usize {
        self.core.network_params.recommended_section_size
    }

    /// Provide a SectionProofSlice that proves the given signature to the given destination.
    pub fn prove(&self, target: &DstLocation) -> Option<SectionProofChain> {
        self.shared_state().map(|state| state.prove(target, None))
    }

    /// If this node is elder and `name` belongs to a member of our section, returns the age
    /// counter of that member. Otherwise returns `None`.
    pub fn member_age_counter(&self, name: &XorName) -> Option<u32> {
        self.stage
            .approved()
            .filter(|stage| stage.is_our_elder(self.core.id()))
            .and_then(|stage| stage.shared_state.our_members.get(name))
            .map(|info| info.age_counter_value())
    }

    /// Returns the latest BLS public key of our section or `None` if we are not joined yet.
    pub fn section_key(&self) -> Option<&bls::PublicKey> {
        self.stage
            .approved()
            .map(|stage| stage.shared_state.our_history.last_key())
    }

    /// Returns our section proof chain, or `None` if we are not joined yet.
    pub fn our_history(&self) -> Option<&SectionProofChain> {
        self.stage
            .approved()
            .map(|stage| &stage.shared_state.our_history)
    }

    pub(crate) fn shared_state(&self) -> Option<&SharedState> {
        self.stage.approved().map(|stage| &stage.shared_state)
    }
}

#[cfg(all(test, feature = "mock"))]
impl Node {
    // Create new node which is already an approved member of a section.
    pub(crate) fn approved(
        config: NodeConfig,
        genesis_prefix_info: GenesisPrefixInfo,
        secret_key_share: Option<bls::SecretKeyShare>,
    ) -> (Self, Receiver<Event>, Receiver<TransportEvent>) {
        let (timer_tx, timer_rx) = crossbeam_channel::unbounded();
        let (transport_tx, transport_node_rx, transport_client_rx) = transport_channels();
        let (user_event_tx, user_event_rx) = crossbeam_channel::unbounded();

        let mut core = Core::new(config, timer_tx, transport_tx, user_event_tx);

        let stage = Approved::new(&mut core, genesis_prefix_info, secret_key_share).unwrap();
        let stage = Stage::Approved(stage);

        let node = Self {
            stage,
            core,
            timer_rx,
            timer_rx_idx: 0,
            transport_rx: transport_node_rx,
            transport_rx_idx: 0,
        };

        (node, user_event_rx, transport_client_rx)
    }

    pub(crate) fn consensus_engine(&self) -> Result<&ConsensusEngine> {
        if let Some(stage) = self.stage.approved() {
            Ok(&stage.consensus_engine)
        } else {
            Err(RoutingError::InvalidState)
        }
    }

    pub(crate) fn consensus_engine_mut(&mut self) -> Result<&mut ConsensusEngine> {
        if let Some(stage) = self.stage.approved_mut() {
            Ok(&mut stage.consensus_engine)
        } else {
            Err(RoutingError::InvalidState)
        }
    }

    pub(crate) fn create_genesis_updates(&self) -> Vec<(P2pNode, AccumulatingMessage)> {
        if let Some(stage) = self.stage.approved() {
            stage.create_genesis_updates()
        } else {
            Vec::new()
        }
    }
}

// Create channels for the network event. Returs a triple of:
// the composite node/client sender, node receiver, client receiver
fn transport_channels() -> (
    EventSenders,
    Receiver<TransportEvent>,
    Receiver<TransportEvent>,
) {
    let (client_tx, client_rx) = crossbeam_channel::unbounded();
    let (node_tx, node_rx) = crossbeam_channel::unbounded();
    (EventSenders { node_tx, client_tx }, node_rx, client_rx)
}
