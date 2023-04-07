use {
    crate::ShredDosError,
    log::info,
    solana_client::rpc_client::RpcClient,
    solana_gossip::gossip_service::{make_gossip_node, spy},
    solana_sdk::{pubkey::Pubkey, signature::Keypair},
    solana_streamer::socket::SocketAddrSpace,
    std::{
        net::SocketAddr,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    },
};

pub(crate) fn get_top_staked_tvu_addrs(
    identity_keypair: Keypair,
    json_rpc_url: &str,
    gossip_entrypoint: &SocketAddr,
    num_nodes: usize,
    allow_private_addr: bool,
) -> Result<Vec<SocketAddr>, ShredDosError> {
    let rpc_client = RpcClient::new(json_rpc_url);
    let mut vote_accounts = rpc_client.get_vote_accounts()?;
    let num_validators = vote_accounts
        .current
        .len()
        .saturating_add(vote_accounts.delinquent.len());
    vote_accounts
        .current
        .sort_unstable_by_key(|k| k.activated_stake);
    let top_staked_vote_accounts = vote_accounts
        .current
        .iter()
        .rev()
        .take(num_nodes)
        .collect::<Vec<_>>();
    let discover_timeout = Duration::from_secs(u64::MAX);
    let socket_addr_space = SocketAddrSpace::new(allow_private_addr);
    let my_shred_version = 0;

    let exit = Arc::new(AtomicBool::new(false));
    let (gossip_service, ip_echo, spy_ref) = make_gossip_node(
        identity_keypair,
        Some(gossip_entrypoint),
        &exit,
        None, // my_gossip_addr,
        my_shred_version,
        true, // should_check_duplicate_instance,
        socket_addr_space,
    );

    let id = spy_ref.id();
    info!("Entrypoint: {:?}", gossip_entrypoint);
    info!("Node Id: {:?}", id);
    let _ip_echo_server = ip_echo
        .map(|tcp_listener| solana_net_utils::ip_echo_server(tcp_listener, Some(my_shred_version)));
    let (_, _, _, validators) = spy(
        spy_ref.clone(),
        Some(num_validators),
        discover_timeout,
        None,
        None,
    );

    let tvu_socket_addrs = top_staked_vote_accounts
        .iter()
        .map(|a| {
            let id = Pubkey::from_str(&a.node_pubkey).unwrap();
            let found = validators.iter().find(|v| v.id == id);
            found.map(|f| f.tvu_forwards).unwrap_or_else(|| {
                // specifically spy for that node if it wasn't found
                let (met_criteria, _, _, retry_validators) =
                    spy(spy_ref.clone(), None, discover_timeout, Some(id), None);
                if met_criteria {
                    retry_validators
                        .iter()
                        .find(|v| v.id == id)
                        .unwrap()
                        .tvu_forwards
                } else {
                    panic!("{} not found", a.node_pubkey)
                }
            })
        })
        .collect::<Vec<_>>();

    exit.store(true, Ordering::Relaxed);
    gossip_service.join().unwrap();

    Ok(tvu_socket_addrs)
}
