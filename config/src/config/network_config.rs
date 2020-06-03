// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::{PersistableConfig, RoleType, RootPath},
    keys::KeyPair,
    utils,
};
use anyhow::{anyhow, ensure, Result};
use libra_crypto::{x25519, Uniform};
use libra_network_address::NetworkAddress;
use libra_types::{transaction::authenticator::AuthenticationKey, PeerId};
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, convert::TryFrom, path::PathBuf, string::ToString};

const NETWORK_PEERS_DEFAULT: &str = "network_peers.config.toml";
const SEED_PEERS_DEFAULT: &str = "seed_peers.toml";

/// Current supported protocol negotiation handshake version.
///
/// See [`perform_handshake`] in `network/src/transport.rs`
// TODO(philiphayes): ideally this constant lives somewhere in network/ ...
// might need to extract into a separate network_constants crate or something.
pub const HANDSHAKE_VERSION: u8 = 0;

#[cfg_attr(any(test, feature = "fuzzing"), derive(Clone, PartialEq))]
#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub peer_id: PeerId,
    // TODO: Add support for multiple listen/advertised addresses in config.
    // The address that this node is listening on for new connections.
    pub listen_address: NetworkAddress,
    // The address that this node advertises to other nodes for the discovery protocol.
    pub advertised_address: NetworkAddress,
    pub discovery_interval_ms: u64,
    pub connectivity_check_interval_ms: u64,
    // Flag to toggle if Noise is used for encryption and authentication.
    pub enable_noise: bool,
    // If the network uses remote authentication, only trusted peers are allowed to connect.
    // Otherwise, any node can connect. If this flag is set to true, `enable_noise` must
    // also be set to true.
    pub enable_remote_authentication: bool,
    // Enable this network to use either gossip discovery or onchain discovery.
    pub discovery_method: DiscoveryMethod,
    // network peers are the nodes allowed to connect when the network is started in authenticated
    // mode.
    #[serde(skip)]
    pub network_peers: NetworkPeersConfig,
    pub network_peers_file: PathBuf,
    // seed_peers act as seed nodes for the discovery protocol.
    #[serde(skip)]
    pub seed_peers: SeedPeersConfig,
    pub seed_peers_file: PathBuf,
    #[serde(rename = "identity_private_key")]
    pub identity_keypair: Option<KeyPair<x25519::PrivateKey>>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            peer_id: PeerId::default(),
            listen_address: "/ip4/0.0.0.0/tcp/6180".parse().unwrap(),
            advertised_address: "/ip4/127.0.0.1/tcp/6180".parse().unwrap(),
            discovery_interval_ms: 1000,
            connectivity_check_interval_ms: 5000,
            enable_noise: true,
            enable_remote_authentication: true,
            discovery_method: DiscoveryMethod::Gossip,
            identity_keypair: None,
            network_peers_file: PathBuf::new(),
            network_peers: NetworkPeersConfig::default(),
            seed_peers_file: PathBuf::new(),
            seed_peers: SeedPeersConfig::default(),
        }
    }
}

impl NetworkConfig {
    /// This clones the underlying data except for the keypair so that this config can be used as a
    /// template for another config.
    pub fn clone_for_template(&self) -> Self {
        Self {
            peer_id: self.peer_id,
            listen_address: self.listen_address.clone(),
            advertised_address: self.advertised_address.clone(),
            discovery_interval_ms: self.discovery_interval_ms,
            connectivity_check_interval_ms: self.connectivity_check_interval_ms,
            enable_noise: self.enable_noise,
            enable_remote_authentication: self.enable_remote_authentication,
            discovery_method: self.discovery_method,
            identity_keypair: None,
            network_peers_file: self.network_peers_file.clone(),
            network_peers: self.network_peers.clone(),
            seed_peers_file: self.seed_peers_file.clone(),
            seed_peers: self.seed_peers.clone(),
        }
    }

    pub fn load(&mut self, root_dir: &RootPath, network_role: RoleType) -> Result<()> {
        if !self.network_peers_file.as_os_str().is_empty() {
            let path = root_dir.full_path(&self.network_peers_file);
            self.network_peers = NetworkPeersConfig::load_config(&path)?;
        }
        if !self.seed_peers_file.as_os_str().is_empty() {
            let path = root_dir.full_path(&self.seed_peers_file);
            self.seed_peers = SeedPeersConfig::load_config(&path)?;
            self.seed_peers.verify_libranet_addrs()?;
        }
        if self.advertised_address.to_string().is_empty() {
            self.advertised_address =
                utils::get_local_ip().ok_or_else(|| anyhow!("No local IP"))?;
        }
        if self.listen_address.to_string().is_empty() {
            self.listen_address = utils::get_local_ip().ok_or_else(|| anyhow!("No local IP"))?;
        }

        if self.enable_remote_authentication {
            ensure!(
                self.enable_noise,
                "For a node to enforce remote authentication, noise must be enabled.",
            );
        }

        if network_role.is_validator() {
            ensure!(
                self.network_peers_file.as_os_str().is_empty(),
                "Validators should not define network_peers_file"
            );
            ensure!(
                self.network_peers.peers.is_empty(),
                "Validators should not define network_peers"
            );
        }

        // TODO(joshlind): investigate the implications of removing these checks.
        if let Some(identity_keypair) = &self.identity_keypair {
            let identity_public_key = identity_keypair.public_key();
            let peer_id = AuthenticationKey::try_from(identity_public_key.as_slice())
                .unwrap()
                .derived_address();

            // If PeerId is not set, derive the PeerId from identity_key.
            if self.peer_id == PeerId::default() {
                self.peer_id = peer_id;
            }
            // Full nodes with remote authentication must derive PeerId from identity_key.
            if !network_role.is_validator() && self.enable_remote_authentication {
                ensure!(
                    self.peer_id == peer_id,
                    "For full-nodes that use remote authentication, \
                    the peer_id must be derived from the identity key.",
                );
            }
        }
        Ok(())
    }

    fn default_path(&self, config_path: &str) -> String {
        format!("{}.{}", self.peer_id.to_string(), config_path)
    }

    pub fn save(&mut self, root_dir: &RootPath) -> Result<()> {
        if self.network_peers != NetworkPeersConfig::default() {
            if self.network_peers_file.as_os_str().is_empty() {
                let file_name = self.default_path(NETWORK_PEERS_DEFAULT);
                self.network_peers_file = PathBuf::from(file_name);
            }
            let path = root_dir.full_path(&self.network_peers_file);
            self.network_peers.save_config(&path)?;
        }

        if self.seed_peers_file.as_os_str().is_empty() {
            let file_name = self.default_path(SEED_PEERS_DEFAULT);
            self.seed_peers_file = PathBuf::from(file_name);
        }
        let path = root_dir.full_path(&self.seed_peers_file);
        self.seed_peers.save_config(&path)?;
        Ok(())
    }

    pub fn random(&mut self, rng: &mut StdRng) {
        self.random_with_peer_id(rng, None);
    }

    pub fn random_with_peer_id(&mut self, rng: &mut StdRng, peer_id: Option<PeerId>) {
        let identity_key = x25519::PrivateKey::generate(rng);
        self.peer_id = if let Some(peer_id) = peer_id {
            peer_id
        } else {
            AuthenticationKey::try_from(identity_key.public_key().as_slice())
                .unwrap()
                .derived_address()
        };
        self.identity_keypair = Some(KeyPair::load(identity_key));
    }
}

// This is separated to another config so that it can be written to its own file
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SeedPeersConfig {
    // All peers config. Key:a unique peer id, will be PK in future, Value: peer discovery info
    pub seed_peers: HashMap<PeerId, Vec<NetworkAddress>>,
}

impl SeedPeersConfig {
    /// Check that all seed peer addresses look like canonical LibraNet addresses
    pub fn verify_libranet_addrs(&self) -> Result<()> {
        for (peer_id, addrs) in self.seed_peers.iter() {
            for addr in addrs {
                ensure!(
                    addr.is_libranet_addr(),
                    "Unexpected seed peer address format: peer_id: {}, addr: '{}'",
                    peer_id.short_str(),
                    addr,
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct NetworkPeersConfig {
    #[serde(flatten)]
    #[serde(serialize_with = "utils::serialize_ordered_map")]
    pub peers: HashMap<PeerId, NetworkPeerInfo>,
}

impl std::fmt::Debug for NetworkPeersConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "<{} keys>", self.peers.len())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NetworkPeerInfo {
    #[serde(rename = "ni")]
    pub identity_public_key: x25519::PublicKey,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryMethod {
    // default until we can deprecate
    Gossip,
    Onchain,
    None,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::config::RoleType;
    use libra_temppath::TempPath;
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn test_with_defaults() {
        // Assert default exists
        let (mut config, path) = generate_config();
        assert_eq!(config.network_peers, NetworkPeersConfig::default());
        assert_eq!(config.network_peers_file, PathBuf::new());
        assert_eq!(config.identity_keypair, None);
        assert_eq!(config.peer_id, PeerId::default());
        assert_eq!(config.seed_peers, SeedPeersConfig::default());
        assert_eq!(config.seed_peers_file, PathBuf::new());

        // Assert default loading doesn't affect paths and defaults remain in place
        let root_dir = RootPath::new_path(path.path());
        config.load(&root_dir, RoleType::FullNode).unwrap();
        assert_eq!(config.network_peers, NetworkPeersConfig::default());
        assert_eq!(config.network_peers_file, PathBuf::new());
        assert_eq!(config.identity_keypair, None);
        assert_eq!(config.peer_id, PeerId::default());
        assert_eq!(config.seed_peers_file, PathBuf::new());
        assert_eq!(config.seed_peers, SeedPeersConfig::default());

        // Assert saving updates paths
        config.save(&root_dir).unwrap();
        assert_eq!(config.seed_peers, SeedPeersConfig::default());
        assert_eq!(
            config.seed_peers_file,
            PathBuf::from(config.default_path(SEED_PEERS_DEFAULT))
        );

        // Assert paths and values are not set (i.e., no defaults apply)
        assert_eq!(config.identity_keypair, None);
        assert_eq!(config.network_peers, NetworkPeersConfig::default());
        assert_eq!(config.network_peers_file, PathBuf::new());
    }

    #[test]
    fn test_with_random() {
        let (mut config, path) = generate_config();
        config.network_peers = NetworkPeersConfig::default();
        let mut rng = StdRng::from_seed([5u8; 32]);
        config.random(&mut rng);
        // This is default (empty) otherwise
        config.seed_peers.seed_peers.insert(config.peer_id, vec![]);

        let keypair = config.identity_keypair.clone();
        let peers = config.network_peers.clone();
        let seed_peers = config.seed_peers.clone();

        // Assert empty paths
        assert_eq!(config.network_peers_file, PathBuf::new());
        assert_eq!(config.seed_peers_file, PathBuf::new());

        // Assert saving updates paths
        let root_dir = RootPath::new_path(path.path());
        config.save(&root_dir).unwrap();
        assert_eq!(config.identity_keypair, keypair);
        assert_eq!(config.network_peers, peers);
        assert_eq!(config.network_peers_file, PathBuf::new(),);
        assert_eq!(config.seed_peers, seed_peers);
        assert_eq!(
            config.seed_peers_file,
            PathBuf::from(config.default_path(SEED_PEERS_DEFAULT))
        );

        // Assert a fresh load correctly populates the config
        let mut new_config = NetworkConfig::default();
        new_config.peer_id = config.peer_id;
        // First that paths are empty
        assert_eq!(new_config.network_peers_file, PathBuf::new());
        assert_eq!(new_config.seed_peers_file, PathBuf::new());
        // Loading populates things correctly
        let result = new_config.load(&root_dir, RoleType::Validator);
        result.unwrap();
        assert_eq!(config.identity_keypair, keypair);
        assert_eq!(config.network_peers, peers);
        assert_eq!(config.network_peers_file, PathBuf::new(),);
        assert_eq!(config.seed_peers, seed_peers);
        assert_eq!(
            config.seed_peers_file,
            PathBuf::from(config.default_path(SEED_PEERS_DEFAULT))
        );
    }

    #[test]
    fn test_default_peer_id() {
        // Generate a random node and verify a distinct peer id
        let (mut config, path) = generate_config();
        let mut rng = StdRng::from_seed([32u8; 32]);
        config.random(&mut rng);
        let root_dir = RootPath::new_path(path.path());

        let default_peer_id = PeerId::default();
        let actual_peer_id = config.peer_id;
        assert!(actual_peer_id != default_peer_id);

        // Now reset and save
        config.peer_id = default_peer_id;
        config.save(&root_dir).unwrap();

        // Now load and verify the distinct peer id
        assert_eq!(config.peer_id, default_peer_id);
        config.load(&root_dir, RoleType::FullNode).unwrap();
        assert_eq!(config.peer_id, actual_peer_id);
    }

    #[test]
    fn test_generate_ip_addresses_on_load() {
        // Generate a random node
        let (mut config, path) = generate_config();
        let mut rng = StdRng::from_seed([32u8; 32]);
        config.random(&mut rng);
        let root_dir = RootPath::new_path(path.path());

        // Now reset IP addresses and save
        config.listen_address = NetworkAddress::mock();
        config.advertised_address = NetworkAddress::mock();
        config.save(&root_dir).unwrap();

        // Now load and verify default IP addresses are generated
        config.load(&root_dir, RoleType::FullNode).unwrap();
        assert_ne!(config.listen_address.to_string(), "");
        assert_ne!(config.advertised_address.to_string(), "");
    }

    fn generate_config() -> (NetworkConfig, TempPath) {
        let temp_dir = TempPath::new();
        temp_dir.create_as_dir().expect("error creating tempdir");
        let mut config = NetworkConfig::default();
        config.network_peers = NetworkPeersConfig::default();
        (config, temp_dir)
    }
}