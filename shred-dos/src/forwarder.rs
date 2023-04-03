#![allow(clippy::integer_arithmetic)] // required for crossbeam_channel::select!
use {
    crate::ShredDosError,
    crossbeam_channel::{Receiver, RecvError},
    log::{debug, error, info},
    solana_metrics::datapoint_info,
    solana_perf::{
        packet::{PacketBatch, PacketBatchRecycler},
        recycler::Recycler,
        sigverify::{dedup_packets_and_count_discards, Deduper},
    },
    solana_streamer::{
        sendmmsg::{batch_send, SendPktsError},
        streamer::{self, StreamerReceiveStats},
    },
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        panic,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        thread::{Builder, JoinHandle},
        time::Duration,
    },
};

pub(crate) const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);
pub(crate) type ShredDeduper = Deduper<2, [u8]>;

/// Bind to ports and start forwarding shreds
#[allow(clippy::too_many_arguments)]
pub fn start_forwarder_threads(
    dest_sockets: Vec<SocketAddr>,
    src_port: u16,
    num_threads: Option<usize>,
    deduper: Arc<RwLock<ShredDeduper>>,
    metrics: Arc<ShredMetrics>,
    shutdown_receiver: Receiver<()>,
    exit: Arc<AtomicBool>,
    packet_hold_duration: Option<Duration>,
) -> Vec<JoinHandle<()>> {
    let num_threads = num_threads
        .unwrap_or_else(|| usize::from(std::thread::available_parallelism().unwrap()).max(8));

    let recycler: PacketBatchRecycler = Recycler::warmed(100, 1024);

    // spawn a thread for each listen socket. linux kernel will load balance amongst shared sockets
    solana_net_utils::multi_bind_in_range(
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        (src_port, src_port.checked_add(1).unwrap()),
        num_threads,
    )
    .unwrap_or_else(|_| {
        panic!("Failed to bind listener sockets. Check that port {src_port} is not in use.")
    })
    .1
    .into_iter()
    .enumerate()
    .flat_map(|(thread_id, incoming_shred_socket)| {
        let (packet_sender, packet_receiver) = crossbeam_channel::unbounded();
        let listen_thread = streamer::receiver(
            Arc::new(incoming_shred_socket),
            exit.clone(),
            packet_sender,
            recycler.clone(),
            Arc::new(StreamerReceiveStats::new("shred_dos-listen_thread")),
            0,     // do not coalesce since batching consumes more cpu cycles and adds latency.
            false, // do not pin memory to use minimal RAM
            None,
        );

        let deduper = deduper.clone();
        let dest_sockets = dest_sockets.clone();
        let metrics = metrics.clone();
        let shutdown_receiver = shutdown_receiver.clone();
        let exit = exit.clone();

        let send_thread = Builder::new()
            .name(format!("shred_dos-send_thread_{thread_id}"))
            .spawn(move || {
                let send_socket =
                    UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
                        .expect("to bind to udp port for forwarding");
                let mut packet_batches = vec![];
                while !exit.load(Ordering::Relaxed) {
                    if let Some(packet_hold_duration) = packet_hold_duration {
                        let send_tick = crossbeam_channel::tick(packet_hold_duration);
                        crossbeam_channel::select! {
                            // send packets
                            recv(send_tick) -> _ => {
                                let res = recv_from_channel_and_send_multiple_dest(
                                    packet_batches,
                                    &deduper,
                                    &send_socket,
                                    &dest_sockets,
                                    &metrics,
                                );

                                // avoid unwrap to prevent log spam from panic handler in each thread
                                if res.is_err(){
                                    break;
                                }
                                packet_batches = vec![];
                            }
                            // receive packets
                            recv(packet_receiver) -> maybe_packet_batch => {
                                packet_batches.push(maybe_packet_batch);
                            }
                            // handle shutdown (avoid using sleep since it will hang under SIGINT)
                            recv(shutdown_receiver) -> _ => {
                                break;
                            }
                        }
                    } else {
                        crossbeam_channel::select! {
                            // forward packets
                            recv(packet_receiver) -> maybe_packet_batch => {
                               let res = recv_from_channel_and_send_multiple_dest(
                                   vec![maybe_packet_batch],
                                   &deduper,
                                   &send_socket,
                                   &dest_sockets,
                                   &metrics,
                               );

                                // avoid unwrap to prevent log spam from panic handler in each thread
                                if res.is_err(){
                                    break;
                                }
                            }
                            // handle shutdown (avoid using sleep since it will hang under SIGINT)
                            recv(shutdown_receiver) -> _ => {
                                break;
                            }
                        }
                    }
                }
                info!("Exiting forwarder thread {thread_id}.");
            })
            .unwrap();

        [listen_thread, send_thread]
    })
    .collect::<Vec<JoinHandle<()>>>()
}

/// broadcasts same packet to multiple recipients
/// for best performance, connect sockets to their destinations
/// Returns Err when unable to receive packets.
fn recv_from_channel_and_send_multiple_dest(
    maybe_packet_batch: Vec<Result<PacketBatch, RecvError>>,
    deduper: &RwLock<ShredDeduper>,
    send_socket: &UdpSocket,
    dest_sockets: &[SocketAddr],
    metrics: &Arc<ShredMetrics>,
) -> Result<(), ShredDosError> {
    let mut packet_batch_vec = maybe_packet_batch
        .into_iter()
        .map(|b| b.map_err(ShredDosError::RecvError))
        .collect::<Result<Vec<_>, _>>()?;

    let num_deduped = dedup_packets_and_count_discards(
        &deduper.read().unwrap(),
        &mut packet_batch_vec,
        |_received_packet, _is_already_marked_as_discard, _is_dup| {},
    );

    let packets_with_dest = dest_sockets
        .iter()
        .flat_map(|outgoing_socketaddr| {
            packet_batch_vec.iter().flat_map(move |packet_batch| {
                metrics
                    .agg_received
                    .fetch_add(packet_batch.len() as u64, Ordering::Relaxed);
                debug!(
                    "Got batch of {} packets, total size in bytes: {}",
                    packet_batch.len(),
                    packet_batch.iter().map(|x| x.meta.size).sum::<usize>()
                );
                packet_batch.iter().filter_map(move |pkt| {
                    let data = pkt.data(..)?;
                    let addr = outgoing_socketaddr;
                    Some((data, addr))
                })
            })
        })
        .collect::<Vec<(&[u8], &SocketAddr)>>();

    match batch_send(send_socket, &packets_with_dest) {
        Ok(_) => {
            metrics
                .agg_success_forward
                .fetch_add(packets_with_dest.len() as u64, Ordering::Relaxed);
            metrics.duplicate.fetch_add(num_deduped, Ordering::Relaxed);
        }
        Err(SendPktsError::IoError(err, num_failed)) => {
            metrics
                .agg_fail_forward
                .fetch_add(packets_with_dest.len() as u64, Ordering::Relaxed);
            error!(
                "Failed to send batch of size {}. {num_failed} packets failed. Error: {err}",
                packets_with_dest.len()
            );
        }
    }

    Ok(())
}

/// Send metrics to influx
pub fn start_forwarder_accessory_thread(
    deduper: Arc<RwLock<ShredDeduper>>,
    metrics: Arc<ShredMetrics>,
    metrics_update_interval_ms: u64,
    shutdown_receiver: Receiver<()>,
    exit: Arc<AtomicBool>,
) -> JoinHandle<()> {
    Builder::new()
        .name("shred_dos-accessory_thread".to_string())
        .spawn(move || {
            let metrics_tick =
                crossbeam_channel::tick(Duration::from_millis(metrics_update_interval_ms));
            let deduper_tick = crossbeam_channel::tick(DEDUPER_RESET_CYCLE);
            while !exit.load(Ordering::Relaxed) {
                crossbeam_channel::select! {
                    // reset deduper to avoid false positives
                    recv(deduper_tick) -> _ => {
                        deduper.write().unwrap().maybe_reset(
                            &mut rand::thread_rng(),
                            DEDUPER_FALSE_POSITIVE_RATE,
                            DEDUPER_RESET_CYCLE,
                        );
                    }
                    // send metrics to influx
                    recv(metrics_tick) -> _ => {
                        metrics.report();
                        metrics.reset();
                    }
                    // handle SIGINT shutdown
                    recv(shutdown_receiver) -> _ => {
                        break;
                    }
                }
            }
        })
        .unwrap()
}

#[derive(Default)]
pub struct ShredMetrics {
    /// Total number of shreds received. Includes duplicates when receiving shreds from multiple regions
    pub agg_received: AtomicU64,
    /// Total number of shreds successfully forwarded, accounting for all destinations
    pub agg_success_forward: AtomicU64,
    /// Total number of shreds failed to forward, accounting for all destinations
    pub agg_fail_forward: AtomicU64,
    /// Total time spent load-shedding
    pub agg_load_shed_us: AtomicU64,
    /// Number of duplicate shreds received
    pub duplicate: AtomicU64,
    pub log_context: Option<String>,

    // cumulative metrics (persist after reset)
    pub agg_received_cumulative: AtomicU64,
    pub agg_success_forward_cumulative: AtomicU64,
    pub agg_fail_forward_cumulative: AtomicU64,
    pub agg_load_shed_us_cumulative: AtomicU64,
    pub duplicate_cumulative: AtomicU64,
}

impl ShredMetrics {
    pub fn new(log_context: Option<String>) -> Self {
        Self {
            log_context,
            ..Self::default()
        }
    }

    pub fn report(&self) {
        if let Some(log_context) = &self.log_context {
            datapoint_info!(
                "shred_dos-connection_metrics",
                ("log_context", log_context, String),
                (
                    "agg_received",
                    self.agg_received.load(Ordering::Relaxed),
                    i64
                ),
                (
                    "agg_success_forward",
                    self.agg_success_forward.load(Ordering::Relaxed),
                    i64
                ),
                (
                    "agg_fail_forward",
                    self.agg_fail_forward.load(Ordering::Relaxed),
                    i64
                ),
                (
                    "agg_load_shed_us",
                    self.agg_load_shed_us.load(Ordering::Relaxed),
                    i64
                ),
                ("duplicate", self.duplicate.load(Ordering::Relaxed), i64),
            );
        }
    }

    /// resets current values, increments cumulative values
    pub fn reset(&self) {
        self.agg_received_cumulative.fetch_add(
            self.agg_received.swap(0, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.agg_success_forward_cumulative.fetch_add(
            self.agg_success_forward.swap(0, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.agg_fail_forward_cumulative.fetch_add(
            self.agg_fail_forward.swap(0, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.agg_load_shed_us_cumulative.fetch_add(
            self.agg_load_shed_us.swap(0, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.duplicate_cumulative
            .fetch_add(self.duplicate.swap(0, Ordering::Relaxed), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::forwarder::{
            recv_from_channel_and_send_multiple_dest, ShredDeduper, ShredMetrics, DEDUPER_NUM_BITS,
        },
        solana_perf::packet::{Meta, Packet, PacketBatch},
        solana_sdk::packet::{PacketFlags, PACKET_DATA_SIZE},
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
            str::FromStr,
            sync::{Arc, Mutex, RwLock},
            thread::{self, sleep},
            time::Duration,
        },
    };

    fn listen_and_collect(listen_socket: UdpSocket, received_packets: Arc<Mutex<Vec<Vec<u8>>>>) {
        let mut buf = [0u8; PACKET_DATA_SIZE];
        loop {
            listen_socket.recv(&mut buf).unwrap();
            received_packets.lock().unwrap().push(Vec::from(buf));
        }
    }

    #[test]
    fn test_2shreds_3destinations() {
        let packet_batch = PacketBatch::new(vec![
            Packet::new(
                [1; PACKET_DATA_SIZE],
                Meta {
                    size: PACKET_DATA_SIZE,
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 48289, // received on random port
                    flags: PacketFlags::empty(),
                    sender_stake: 0,
                },
            ),
            Packet::new(
                [2; PACKET_DATA_SIZE],
                Meta {
                    size: PACKET_DATA_SIZE,
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 9999,
                    flags: PacketFlags::empty(),
                    sender_stake: 0,
                },
            ),
        ]);
        let (packet_sender, packet_receiver) = crossbeam_channel::unbounded::<PacketBatch>();
        packet_sender.send(packet_batch).unwrap();

        let dest_socketaddrs = vec![
            SocketAddr::from_str("0.0.0.0:32881").unwrap(),
            SocketAddr::from_str("0.0.0.0:33881").unwrap(),
            SocketAddr::from_str("0.0.0.0:34881").unwrap(),
        ];

        let test_listeners = dest_socketaddrs
            .iter()
            .map(|socketaddr| {
                (
                    UdpSocket::bind(socketaddr).unwrap(),
                    *socketaddr,
                    // store results in vec of packet, where packet is Vec<u8>
                    Arc::new(Mutex::new(vec![])),
                )
            })
            .collect::<Vec<_>>();

        let udp_sender = UdpSocket::bind("0.0.0.0:10000").unwrap();

        // spawn listeners
        test_listeners
            .iter()
            .for_each(|(listen_socket, _socketaddr, to_receive)| {
                let socket = listen_socket.try_clone().unwrap();
                let to_receive = to_receive.to_owned();
                thread::spawn(move || listen_and_collect(socket, to_receive));
            });

        // send packets
        recv_from_channel_and_send_multiple_dest(
            vec![packet_receiver.recv()],
            &RwLock::new(ShredDeduper::new(&mut rand::thread_rng(), DEDUPER_NUM_BITS)),
            &udp_sender,
            &Arc::new(dest_socketaddrs),
            &Arc::new(ShredMetrics::new(None)),
        )
        .unwrap();

        // allow packets to be received
        sleep(Duration::from_millis(500));

        let received = test_listeners
            .iter()
            .map(|(_, _, results)| results.clone())
            .collect::<Vec<_>>();

        // check results
        for received in received.iter() {
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 2);
            assert!(received
                .iter()
                .all(|packet| packet.len() == PACKET_DATA_SIZE));
            assert_eq!(received[0], [1; PACKET_DATA_SIZE]);
            assert_eq!(received[1], [2; PACKET_DATA_SIZE]);
        }

        assert_eq!(
            received
                .iter()
                .fold(0, |acc, elem| acc + elem.lock().unwrap().len()),
            6
        );
    }

    #[test]
    fn load_shedding() {
        const CHANNEL_SIZE: usize = 1_000;
        let (sender, receiver) = crossbeam_channel::bounded(CHANNEL_SIZE);

        // fill the channel
        for i in 0..CHANNEL_SIZE {
            sender.send(i).unwrap();
        }

        // load shed
        assert!(receiver.is_full());
        for _ in 0..(CHANNEL_SIZE - CHANNEL_SIZE / 100) {
            let _ = receiver.try_recv();
        }
        assert_eq!(receiver.try_recv().unwrap(), 990);
    }
}
