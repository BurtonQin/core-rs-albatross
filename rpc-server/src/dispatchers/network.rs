use std::sync::Arc;

use async_trait::async_trait;

use nimiq_network_interface::network::Network as InterfaceNetwork;
use nimiq_network_libp2p::Network;
use nimiq_rpc_interface::network::NetworkInterface;
use nimiq_rpc_interface::types::RPCResult;

use crate::error::Error;

pub struct NetworkDispatcher {
    network: Arc<Network>,
}

impl NetworkDispatcher {
    pub fn new(network: Arc<Network>) -> Self {
        NetworkDispatcher { network }
    }
}

#[nimiq_jsonrpc_derive::service(rename_all = "camelCase")]
#[async_trait]
impl NetworkInterface for NetworkDispatcher {
    type Error = Error;

    /// Returns the peer ID for our local peer.
    async fn get_peer_id(&mut self) -> RPCResult<String, (), Self::Error> {
        Ok(self.network.local_peer_id().to_string().into())
    }

    /// Returns the number of peers.
    async fn get_peer_count(&mut self) -> RPCResult<usize, (), Self::Error> {
        Ok(self.network.get_peers().len().into())
    }

    /// Returns a list with the IDs of all our peers.
    async fn get_peer_list(&mut self) -> RPCResult<Vec<String>, (), Self::Error> {
        Ok(self
            .network
            .get_peers()
            .into_iter()
            .map(|peer_id| peer_id.to_string())
            .collect::<Vec<_>>()
            .into())
    }
}
