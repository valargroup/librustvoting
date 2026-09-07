//! Reports the NU6.3 (Ironwood) activation height, which bounds how far a
//! voter wallet must scan: no votable note can exist before it.
use zcash_protocol::consensus::{NetworkUpgrade, Parameters};
use zcash_voting::Network;

fn main() {
    for network in [Network::Testnet, Network::Mainnet] {
        let height = network.activation_height(NetworkUpgrade::Nu6_3);
        println!("{network:?}: NU6.3 activates at {height:?}");
    }
}
