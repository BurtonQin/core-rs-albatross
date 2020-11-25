#![allow(dead_code)]

use std::{collections::HashMap, pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures::{
    channel::{mpsc, oneshot},
    lock::Mutex as AsyncMutex,
    FutureExt, SinkExt, Stream, StreamExt,
};
use libp2p::{
    core,
    core::{muxing::StreamMuxerBox, transport::Boxed},
    dns,
    gossipsub::{GossipsubConfig, GossipsubEvent, GossipsubMessage, Topic as GossipsubTopic, TopicHash},
    identity::Keypair,
    kad::{GetRecordOk, KademliaConfig, KademliaEvent, QueryId, QueryResult, Quorum, Record},
    noise,
    swarm::{SwarmBuilder, SwarmEvent},
    tcp, websocket, yamux, Multiaddr, PeerId, Swarm, Transport,
};
use thiserror::Error;
use tokio::sync::broadcast;

#[cfg(test)]
use libp2p::core::transport::MemoryTransport;

use beserial::{Deserialize, Serialize};
use nimiq_network_interface::{
    network::{Network as NetworkInterface, NetworkEvent, Topic},
    peer_map::ObservablePeerMap,
};

use crate::{
    behaviour::{NimiqBehaviour, NimiqEvent, NimiqNetworkBehaviourError},
    discovery::{behaviour::DiscoveryConfig, peer_contacts::PeerContact},
    limit::behaviour::LimitConfig,
    message::behaviour::MessageConfig,
    message::peer::Peer,
};

pub struct Config {
    pub keypair: Keypair,

    pub peer_contact: PeerContact,

    pub discovery: DiscoveryConfig,
    pub message: MessageConfig,
    pub limit: LimitConfig,
    pub kademlia: KademliaConfig,
    pub gossipsub: GossipsubConfig,
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("Dial error: {0}")]
    Dial(#[from] libp2p::swarm::DialError),

    #[error("Failed to send action to swarm task: {0}")]
    Send(#[from] futures::channel::mpsc::SendError),

    #[error("Network action was cancelled: {0}")]
    Canceled(#[from] futures::channel::oneshot::Canceled),

    #[error("Serialization error: {0}")]
    Serialization(#[from] beserial::SerializingError),

    #[error("Network behaviour error: {0}")]
    Behaviour(#[from] NimiqNetworkBehaviourError),

    #[error("DHT store error: {0:?}")]
    DhtStore(libp2p::kad::store::Error),

    #[error("DHT GetRecord error: {0:?}")]
    DhtGetRecord(libp2p::kad::GetRecordError),

    #[error("DHT PutRecord error: {0:?}")]
    DhtPutRecord(libp2p::kad::PutRecordError),

    #[error("Gossipsub Publish error: {0:?}")]
    GossipsubPublish(libp2p::gossipsub::error::PublishError),
}

impl From<libp2p::kad::store::Error> for NetworkError {
    fn from(e: libp2p::kad::store::Error) -> Self {
        Self::DhtStore(e)
    }
}

impl From<libp2p::kad::GetRecordError> for NetworkError {
    fn from(e: libp2p::kad::GetRecordError) -> Self {
        Self::DhtGetRecord(e)
    }
}

impl From<libp2p::kad::PutRecordError> for NetworkError {
    fn from(e: libp2p::kad::PutRecordError) -> Self {
        Self::DhtPutRecord(e)
    }
}

impl From<libp2p::gossipsub::error::PublishError> for NetworkError {
    fn from(e: libp2p::gossipsub::error::PublishError) -> Self {
        Self::GossipsubPublish(e)
    }
}

type NimiqSwarm = Swarm<NimiqBehaviour>;
#[derive(Debug)]
pub enum NetworkAction {
    Dial {
        peer_id: PeerId,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    DialAddress {
        address: Multiaddr,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    DhtGet {
        key: Vec<u8>,
        output: oneshot::Sender<Result<Option<Vec<u8>>, NetworkError>>,
    },
    DhtPut {
        key: Vec<u8>,
        value: Vec<u8>,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
    Subscribe {
        topic_name: String,
        output: mpsc::Sender<(GossipsubMessage, PeerId)>,
    },
    Publish {
        topic_name: String,
        data: Vec<u8>,
        output: oneshot::Sender<Result<(), NetworkError>>,
    },
}

#[derive(Default)]
struct TaskState {
    dht_puts: HashMap<QueryId, oneshot::Sender<Result<(), NetworkError>>>,
    dht_gets: HashMap<QueryId, oneshot::Sender<Result<Option<Vec<u8>>, NetworkError>>>,
    gossip_topics: HashMap<TopicHash, mpsc::Sender<(GossipsubMessage, PeerId)>>,
}

pub struct Network {
    local_peer_id: PeerId,
    events_tx: broadcast::Sender<NetworkEvent<Peer>>,
    action_tx: AsyncMutex<mpsc::Sender<NetworkAction>>,
    peers: ObservablePeerMap<Peer>,
}

impl Network {
    /// Create a new libp2p network instance.
    ///
    /// # Arguments
    ///
    ///  - `listen_addr`: The multi-address on which to listen for inbound connections.
    ///  - `config`: The network configuration, containing key pair, and other behaviour-specific configuration.
    ///
    pub fn new(listen_addr: Multiaddr, config: Config) -> Self {
        assert!(!config.gossipsub.hash_topics, "Hash topics not supported");

        let swarm = Self::new_swarm(listen_addr, config);
        let peers = swarm.message.peers.clone();

        let local_peer_id = Swarm::local_peer_id(&swarm).clone();

        let (events_tx, _) = broadcast::channel(64);
        let (action_tx, action_rx) = mpsc::channel(64);

        async_std::task::spawn(Self::swarm_task(swarm, events_tx.clone(), action_rx));

        Self {
            local_peer_id,
            events_tx,
            action_tx: AsyncMutex::new(action_tx),
            peers,
        }
    }

    fn new_transport(keypair: &Keypair) -> std::io::Result<Boxed<(PeerId, StreamMuxerBox)>> {
        let transport = {
            // Websocket over TCP/DNS
            let transport = websocket::WsConfig::new(dns::DnsConfig::new(tcp::TcpConfig::new().nodelay(true))?);

            // Memory transport for testing
            // TODO: Use websocket over the memory transport
            #[cfg(test)]
            let transport = transport.or_transport(MemoryTransport::default());

            transport
        };

        let noise_keys = noise::Keypair::<noise::X25519Spec>::new().into_authentic(keypair).unwrap();

        Ok(transport
            .upgrade(core::upgrade::Version::V1)
            .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
            .multiplex(yamux::YamuxConfig::default())
            .timeout(std::time::Duration::from_secs(20))
            .boxed())
    }

    fn new_swarm(listen_addr: Multiaddr, config: Config) -> Swarm<NimiqBehaviour> {
        let local_peer_id = PeerId::from(config.keypair.clone().public());

        let transport = Self::new_transport(&config.keypair).unwrap();

        let behaviour = NimiqBehaviour::new(config);

        // TODO add proper config
        let mut swarm = SwarmBuilder::new(transport, behaviour, local_peer_id)
            .incoming_connection_limit(5)
            .outgoing_connection_limit(2)
            .peer_connection_limit(1)
            .build();

        Swarm::listen_on(&mut swarm, listen_addr).expect("Failed to listen on provided address");

        swarm
    }

    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    async fn swarm_task(mut swarm: NimiqSwarm, events_tx: broadcast::Sender<NetworkEvent<Peer>>, mut action_rx: mpsc::Receiver<NetworkAction>) {
        let mut task_state = TaskState::default();

        loop {
            futures::select! {
                event = swarm.next_event().fuse() => {
                    log::debug!("Swarm task received event: {:?}", event);
                    Self::handle_event(event, &events_tx, &mut swarm, &mut task_state).await;
                },
                action_opt = action_rx.next().fuse() => {
                    if let Some(action) = action_opt {
                        Self::perform_action(action, &mut swarm, &mut task_state).await.unwrap();
                    }
                    else {
                        // `action_rx.next()` will return `None` if all senders (i.e. the `Network` object) are dropped.
                        break;
                    }
                },
            };
        }
    }

    async fn handle_event(
        event: SwarmEvent<NimiqEvent, NimiqNetworkBehaviourError>,
        events_tx: &broadcast::Sender<NetworkEvent<Peer>>,
        swarm: &mut NimiqSwarm,
        state: &mut TaskState,
    ) {
        match event {
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                swarm.kademlia.add_address(&peer_id, endpoint.get_remote_address().clone());
            }

            //SwarmEvent::ConnectionClosed { .. } => {},
            SwarmEvent::Behaviour(event) => {
                match event {
                    NimiqEvent::Message(event) => {
                        if let Err(event) = events_tx.send(event) {
                            log::error!("Failed to notify subscribers about network event: {:?}", event);
                        }
                    }
                    NimiqEvent::Dht(event) => {
                        match event {
                            KademliaEvent::QueryResult { id, result, .. } => {
                                match result {
                                    QueryResult::GetRecord(result) => {
                                        if let Some(output) = state.dht_gets.remove(&id) {
                                            let result = result.map_err(Into::into).and_then(|GetRecordOk { mut records }| {
                                                // TODO: What do we do, if we get multiple records?
                                                let data_opt = records.pop().map(|r| r.record.value);
                                                Ok(data_opt)
                                            });
                                            output.send(result).ok();
                                        } else {
                                            log::warn!("GetRecord query result for unknown query ID: {:?}", id);
                                        }
                                    }
                                    QueryResult::PutRecord(result) => {
                                        // dht_put resolved
                                        if let Some(output) = state.dht_puts.remove(&id) {
                                            output.send(result.map(|_| ()).map_err(Into::into)).ok();
                                        } else {
                                            log::warn!("PutRecord query result for unknown query ID: {:?}", id);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                    NimiqEvent::Gossip(event) => {
                        match event {
                            GossipsubEvent::Message(peer_id, msg_id, msg) => {
                                log::debug!("Received message {:?} from peer {:?}: {:?}", msg_id, peer_id, msg);
                                for topic in msg.topics.iter() {
                                    if let Some(output) = state.gossip_topics.get_mut(&topic) {
                                        output.send((msg.clone(), peer_id.clone())).await.ok();
                                    } else {
                                        log::warn!("Unknown topic hash: {:?}", topic);
                                    }
                                }
                            }
                            GossipsubEvent::Subscribed { peer_id, topic } => {
                                log::debug!("Peer {:?} subscribed to topic: {:?}", peer_id, topic);
                                /*if let Some(output) = state.gossip_topics.remove(&topic) {
                                    output.send(topic).ok();
                                }*/
                            }
                            GossipsubEvent::Unsubscribed { peer_id, topic } => {
                                log::debug!("Peer {:?} unsubscribed to topic: {:?}", peer_id, topic);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn perform_action(action: NetworkAction, swarm: &mut NimiqSwarm, state: &mut TaskState) -> Result<(), NetworkError> {
        log::debug!("Swarm task: performing action: {:?}", action);

        match action {
            NetworkAction::Dial { peer_id, output } => {
                output.send(Swarm::dial(swarm, &peer_id).map_err(Into::into)).ok();
            }
            NetworkAction::DialAddress { address, output } => {
                output
                    .send(Swarm::dial_addr(swarm, address).map_err(|l| NetworkError::Dial(libp2p::swarm::DialError::ConnectionLimit(l))))
                    .ok();
            }
            NetworkAction::DhtGet { key, output } => {
                let query_id = swarm.kademlia.get_record(&key.into(), Quorum::One);
                state.dht_gets.insert(query_id, output);
            }
            NetworkAction::DhtPut { key, value, output } => {
                let local_peer_id = Swarm::local_peer_id(&swarm);

                let record = Record {
                    key: key.into(),
                    value,
                    publisher: Some(local_peer_id.clone()),
                    expires: None, // TODO: Records should expire at some point in time
                };

                match swarm.kademlia.put_record(record, Quorum::One) {
                    Ok(query_id) => {
                        // Remember put operation to resolve when we receive a `QueryResult::PutRecord`
                        state.dht_puts.insert(query_id, output);
                    }
                    Err(e) => {
                        output.send(Err(e.into())).ok();
                    }
                }
            }
            NetworkAction::Subscribe { topic_name, output } => {
                let topic = GossipsubTopic::new(topic_name.clone());
                if swarm.gossipsub.subscribe(topic.clone()) {
                    state.gossip_topics.insert(topic.no_hash(), output);
                } else {
                    log::warn!("Already subscribed to topic: {:?}", topic_name);
                }
            }
            NetworkAction::Publish { topic_name, data, output } => {
                let topic = GossipsubTopic::new(topic_name);
                output.send(swarm.gossipsub.publish(&topic, data).map_err(Into::into)).ok();
            }
        }

        Ok(())
    }
}

#[async_trait]
impl NetworkInterface for Network {
    type PeerType = Peer;
    type AddressType = Multiaddr;
    type Error = NetworkError;

    fn get_peer_updates(&self) -> (Vec<Arc<Self::PeerType>>, broadcast::Receiver<NetworkEvent<Self::PeerType>>) {
        self.peers.subscribe()
    }

    fn get_peers(&self) -> Vec<Arc<Self::PeerType>> {
        self.peers.get_peers()
    }

    fn get_peer(&self, peer_id: PeerId) -> Option<Arc<Self::PeerType>> {
        self.peers.get_peer(&peer_id)
    }

    fn subscribe_events(&self) -> broadcast::Receiver<NetworkEvent<Self::PeerType>> {
        self.events_tx.subscribe()
    }

    async fn subscribe<T>(&self, topic: &T) -> Result<Pin<Box<dyn Stream<Item = (T::Item, PeerId)> + Send>>, Self::Error>
    where
        T: Topic + Sync,
    {
        let (tx, rx) = mpsc::channel(16);

        self.action_tx
            .lock()
            .await
            .send(NetworkAction::Subscribe {
                topic_name: topic.topic(),
                output: tx,
            })
            .await?;

        Ok(rx
            .map(|(msg, peer_id)| {
                let item: <T as Topic>::Item = Deserialize::deserialize_from_vec(&msg.data).unwrap();
                (item, peer_id)
            })
            .boxed())
    }

    async fn publish<T>(&self, topic: &T, item: <T as Topic>::Item) -> Result<(), Self::Error>
    where
        T: Topic + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();

        let mut buf = vec![];
        item.serialize(&mut buf)?;

        self.action_tx
            .lock()
            .await
            .send(NetworkAction::Publish {
                topic_name: topic.topic(),
                data: buf,
                output: output_tx,
            })
            .await?;

        output_rx.await?
    }

    async fn dht_get<K, V>(&self, k: &K) -> Result<Option<V>, Self::Error>
    where
        K: AsRef<[u8]> + Send + Sync,
        V: Deserialize + Send + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();
        self.action_tx
            .lock()
            .await
            .send(NetworkAction::DhtGet {
                key: k.as_ref().to_owned(),
                output: output_tx,
            })
            .await?;

        if let Some(data) = output_rx.await?? {
            Ok(Some(Deserialize::deserialize_from_vec(&data)?))
        } else {
            Ok(None)
        }
    }

    async fn dht_put<K, V>(&self, k: &K, v: &V) -> Result<(), Self::Error>
    where
        K: AsRef<[u8]> + Send + Sync,
        V: Serialize + Send + Sync,
    {
        let (output_tx, output_rx) = oneshot::channel();

        let mut buf = vec![];
        v.serialize(&mut buf)?;

        self.action_tx
            .lock()
            .await
            .send(NetworkAction::DhtPut {
                key: k.as_ref().to_owned(),
                value: buf,
                output: output_tx,
            })
            .await?;
        output_rx.await?
    }

    async fn dial_peer(&self, peer_id: PeerId) -> Result<(), NetworkError> {
        let (output_tx, output_rx) = oneshot::channel();
        self.action_tx.lock().await.send(NetworkAction::Dial { peer_id, output: output_tx }).await?;
        output_rx.await?
    }

    async fn dial_address(&self, address: Multiaddr) -> Result<(), NetworkError> {
        let (output_tx, output_rx) = oneshot::channel();
        self.action_tx
            .lock()
            .await
            .send(NetworkAction::DialAddress { address, output: output_tx })
            .await?;
        output_rx.await?
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{Stream, StreamExt};
    use libp2p::{
        gossipsub::GossipsubConfig,
        identity::Keypair,
        multiaddr::{multiaddr, Multiaddr},
        swarm::KeepAlive,
        PeerId,
    };
    use rand::{thread_rng, Rng};

    use beserial::{Deserialize, Serialize};
    use nimiq_network_interface::{
        message::Message,
        network::Network as NetworkInterface,
        peer::{CloseReason, Peer as PeerInterface},
    };

    use super::{Config, Network};
    use crate::{
        discovery::{
            behaviour::DiscoveryConfig,
            peer_contacts::{PeerContact, Protocols, Services},
        },
        message::peer::Peer,
    };
    use nimiq_network_interface::network::{NetworkEvent, Topic};

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct TestMessage {
        id: u32,
    }

    impl Message for TestMessage {
        const TYPE_ID: u64 = 42;
    }

    fn network_config(address: Multiaddr) -> Config {
        let keypair = Keypair::generate_ed25519();

        let mut peer_contact = PeerContact {
            addresses: vec![address],
            public_key: keypair.public().clone(),
            services: Services::all(),
            timestamp: None,
        };
        peer_contact.set_current_time();

        let mut gossipsub = GossipsubConfig::default();
        gossipsub.mesh_n = 2;
        gossipsub.mesh_n_low = 2;

        Config {
            keypair,
            peer_contact,
            discovery: DiscoveryConfig {
                genesis_hash: Default::default(),
                update_interval: Duration::from_secs(60),
                min_recv_update_interval: Duration::from_secs(30),
                update_limit: 64,
                protocols_filter: Protocols::all(),
                services_filter: Services::all(),
                min_send_update_interval: Duration::from_secs(30),
                house_keeping_interval: Duration::from_secs(60),
                keep_alive: KeepAlive::No,
            },
            message: Default::default(),
            limit: Default::default(),
            kademlia: Default::default(),
            gossipsub,
        }
    }

    fn assert_peer_joined(event: &NetworkEvent<Peer>, peer_id: &PeerId) {
        if let NetworkEvent::PeerJoined(peer) = event {
            assert_eq!(&peer.id, peer_id);
        } else {
            panic!("Event is not a NetworkEvent::PeerJoined: {:?}", event);
        }
    }

    #[derive(Clone, Debug)]
    struct TestNetwork {
        next_address: u64,
        addresses: Vec<Multiaddr>,
    }

    impl TestNetwork {
        pub fn new() -> Self {
            Self {
                next_address: thread_rng().gen::<u64>(),
                addresses: vec![],
            }
        }

        pub async fn spawn(&mut self) -> Network {
            let address = multiaddr![Memory(self.next_address)];
            self.next_address += 1;

            let net = Network::new(address.clone(), network_config(address.clone()));
            log::info!("Creating node: address={}, peer_id={}", address, net.local_peer_id);

            if let Some(dial_address) = self.addresses.first() {
                log::info!("Dialing peer: address={}", dial_address);
                net.dial_address(dial_address.clone()).await.unwrap();
            }

            let mut events = net.subscribe_events();
            log::debug!("event: {:?}", events.next().await);

            self.addresses.push(address);

            net
        }

        pub async fn spawn_2() -> (Network, Network) {
            let mut net = Self::new();

            let net1 = net.spawn().await;
            let net2 = net.spawn().await;

            (net1, net2)
        }
    }

    async fn create_connected_networks() -> (Network, Network) {
        log::info!("Creating connected test networks:");
        let addr1 = multiaddr![Memory(thread_rng().gen::<u64>())];
        let addr2 = multiaddr![Memory(thread_rng().gen::<u64>())];

        let net1 = Network::new(addr1.clone(), network_config(addr1.clone()));
        let net2 = Network::new(addr2.clone(), network_config(addr2.clone()));

        log::info!("Network 1: address={}, peer_id={}", addr1, net1.local_peer_id);
        log::info!("Network 2: address={}, peer_id={}", addr2, net2.local_peer_id);

        log::info!("Dialing peer 1 from peer 2...");
        net2.dial_address(addr1).await.unwrap();

        let mut events1 = net1.subscribe_events();
        let mut events2 = net2.subscribe_events();

        log::info!("Waiting for events");

        let event1 = events1.next().await.unwrap().unwrap();
        log::debug!("event1 = {:?}", event1);
        assert_peer_joined(&event1, &net2.local_peer_id);

        let event2 = events2.next().await.unwrap().unwrap();
        log::debug!("event2 = {:?}", event2);
        assert_peer_joined(&event2, &net1.local_peer_id);

        (net1, net2)
    }

    #[tokio::test]
    async fn two_networks_can_connect() {
        let (net1, net2) = create_connected_networks().await;
        assert_eq!(net1.get_peers().len(), 1);
        assert_eq!(net2.get_peers().len(), 1);

        let peer2 = net1.get_peer(net2.local_peer_id().clone()).unwrap();
        let peer1 = net2.get_peer(net1.local_peer_id().clone()).unwrap();
        assert_eq!(peer2.id(), net2.local_peer_id);
        assert_eq!(peer1.id(), net1.local_peer_id);

        log::info!("Test finished");
    }

    #[tokio::test]
    async fn one_peer_can_talk_to_another() {
        let (net1, net2) = create_connected_networks().await;

        let peer2 = net1.get_peer(net2.local_peer_id().clone()).unwrap();
        let peer1 = net2.get_peer(net1.local_peer_id().clone()).unwrap();

        let mut msgs = peer1.receive::<TestMessage>();

        peer2.send(&TestMessage { id: 4711 }).await.unwrap();

        log::info!("Send complete");

        let msg = msgs.next().await.unwrap();

        assert_eq!(msg.id, 4711);
    }

    #[tokio::test]
    async fn both_peers_can_talk_with_each_other() {
        let (net1, net2) = create_connected_networks().await;

        let peer2 = net1.get_peer(net2.local_peer_id().clone()).unwrap();
        let peer1 = net2.get_peer(net1.local_peer_id().clone()).unwrap();

        let mut in1 = peer1.receive::<TestMessage>();
        let mut in2 = peer2.receive::<TestMessage>();

        peer1.send(&TestMessage { id: 1337 }).await.unwrap();
        peer2.send(&TestMessage { id: 420 }).await.unwrap();

        let msg1 = in2.next().await.unwrap();
        let msg2 = in1.next().await.unwrap();

        assert_eq!(msg1.id, 1337);
        assert_eq!(msg2.id, 420);
    }

    fn assert_peer_left(event: &NetworkEvent<Peer>, peer_id: &PeerId) {
        if let NetworkEvent::PeerLeft(peer) = event {
            assert_eq!(&peer.id, peer_id);
        } else {
            panic!("Event is not a NetworkEvent::PeerLeft: {:?}", event);
        }
    }

    #[tokio::test]
    async fn connections_are_properly_closed() {
        //env_logger::init();

        let (net1, net2) = create_connected_networks().await;

        let peer2 = net1.get_peer(net2.local_peer_id().clone()).unwrap();

        let mut events1 = net1.subscribe_events();
        let mut events2 = net2.subscribe_events();

        peer2.close(CloseReason::Other);

        let event1 = events1.next().await.unwrap().unwrap();
        assert_peer_left(&event1, net2.local_peer_id());
        log::debug!("event1 = {:?}", event1);

        let event2 = events2.next().await.unwrap().unwrap();
        assert_peer_left(&event2, net1.local_peer_id());
        log::debug!("event2 = {:?}", event2);

        assert_eq!(net1.get_peers().len(), 0);
        assert_eq!(net2.get_peers().len(), 0);
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct TestRecord {
        x: i32,
    }

    #[tokio::test]
    async fn dht_put_and_get() {
        let (net1, net2) = create_connected_networks().await;

        let put_record = TestRecord { x: 420 };

        net1.dht_put(b"foo", &put_record).await.unwrap();

        let fetched_record = net2.dht_get::<_, TestRecord>(b"foo").await.unwrap();

        assert_eq!(fetched_record, Some(put_record));
    }

    pub struct TestTopic;

    impl Topic for TestTopic {
        type Item = TestRecord;

        fn topic(&self) -> String {
            "hello_world".to_owned()
        }
    }

    fn consume_stream<T: std::fmt::Debug>(mut stream: impl Stream<Item = T> + Unpin + Send + 'static) {
        tokio::spawn(async move { while let Some(_) = stream.next().await {} });
    }

    #[tokio::test]
    async fn test_gossipsub() {
        let mut net = TestNetwork::new();

        let net1 = net.spawn().await;
        let net2 = net.spawn().await;

        for _ in 0..5i32 {
            let net_n = net.spawn().await;
            net_n.subscribe_events().next().await;
            let stream_n = net_n.subscribe(&TestTopic).await.unwrap();
            consume_stream(stream_n);
        }

        let test_message = TestRecord { x: 42 };

        let mut messages = net1.subscribe(&TestTopic).await.unwrap();
        consume_stream(net2.subscribe(&TestTopic).await.unwrap());

        tokio::time::delay_for(Duration::from_secs(10)).await;

        net2.publish(&TestTopic, test_message.clone()).await.unwrap();

        let (received_message, _peer) = messages.next().await.unwrap();
        log::info!("Received GossipSub message: {:?}", received_message);

        assert_eq!(received_message, test_message);
    }
}