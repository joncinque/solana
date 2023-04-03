use {
    crate::forwarder::{ShredDeduper, ShredMetrics, DEDUPER_NUM_BITS},
    clap::{arg, Parser},
    crossbeam_channel::{Receiver, RecvError, Sender},
    gethostname::gethostname,
    log::*,
    signal_hook::consts::{SIGINT, SIGTERM},
    solana_metrics::set_host_id,
    std::{
        io,
        net::{IpAddr, SocketAddr},
        panic,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, sleep},
        time::Duration,
    },
    thiserror::Error,
};

mod forwarder;

#[derive(Clone, Debug, Parser)]
#[clap(author, version, about, long_about = None)]
// https://docs.rs/clap/latest/clap/_derive/_cookbook/git_derive/index.html
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Debug, clap::Subcommand)]
enum Commands {
    /// Sends anything received on `src-bind-addr`:`src-bind-port` to all destinations.
    Forward(ForwardArgs),
}

#[derive(clap::Args, Clone, Debug)]
struct ForwardArgs {
    /// Address where shred-dos listens.
    #[arg(long, env, default_value_t = IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)))]
    src_bind_addr: IpAddr,

    /// Port where shred-dos listens. Use `0` for random ephemeral port.
    #[arg(long, env, default_value_t = 20_000)]
    src_bind_port: u16,

    /// Static set of IP:Port where shred-dos forwards shreds to, comma separated.
    /// Eg. `127.0.0.1:8002,10.0.0.1:8002`.
    #[arg(long, env, value_delimiter = ',')]
    dest_ip_ports: Vec<SocketAddr>,

    /// Log context to add extra info
    #[arg(long, env)]
    log_context: Option<String>,

    /// Interval between logging stats to CLI and influx
    #[arg(long, env, default_value_t = 15_000)]
    metrics_report_interval_ms: u64,

    /// Number of threads to use. Defaults to use all cores, minimum of 8.
    #[arg(long, env)]
    num_threads: Option<usize>,

    /// Time to hold packets before sending out
    #[arg(long, env)]
    packet_hold_ms: Option<u64>,
}

#[derive(Debug, Error)]
pub enum ShredDosError {
    #[error("RecvError {0}")]
    RecvError(#[from] RecvError),
}

// Creates a channel that gets a message every time `SIGINT` is signalled.
fn shutdown_notifier(exit: Arc<AtomicBool>) -> io::Result<(Sender<()>, Receiver<()>)> {
    let (s, r) = crossbeam_channel::bounded(256);
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;

    let s_thread = s.clone();
    thread::spawn(move || {
        for _ in signals.forever() {
            exit.store(true, Ordering::SeqCst);
            // send shutdown signal multiple times since crossbeam doesn't have broadcast channels
            // each thread will consume a shutdown signal
            for _ in 0..256 {
                if s_thread.send(()).is_err() {
                    break;
                }
            }
        }
    });

    Ok((s, r))
}

fn main() -> Result<(), ShredDosError> {
    env_logger::builder().init();
    let all_args: Args = Args::parse();
    let Commands::Forward(args) = all_args.command;
    set_host_id(gethostname().into_string().unwrap());
    if args.dest_ip_ports.is_empty() {
        panic!("No destinations found. You must provide values for --dest-ip-ports or --endpoint-discovery-url.")
    }

    let exit = Arc::new(AtomicBool::new(false));
    let (shutdown_sender, shutdown_receiver) =
        shutdown_notifier(exit.clone()).expect("Failed to set up signal handler");
    let panic_hook = panic::take_hook();
    {
        let exit = exit.clone();
        panic::set_hook(Box::new(move |panic_info| {
            exit.store(true, Ordering::SeqCst);
            let _ = shutdown_sender.send(());
            error!("exiting process");
            sleep(Duration::from_secs(1));
            // invoke the default handler and exit the process
            panic_hook(panic_info);
        }));
    }

    let mut thread_handles = vec![];

    // share deduper + metrics between forwarder <-> accessory thread
    let deduper = Arc::new(RwLock::new(ShredDeduper::new(
        &mut rand::thread_rng(),
        DEDUPER_NUM_BITS,
    )));

    // use mutex since metrics are write heavy. cheaper than rwlock
    let metrics = Arc::new(ShredMetrics::new(args.log_context));

    let packet_hold_duration = args.packet_hold_ms.map(Duration::from_millis);
    let forwarder_hdls = forwarder::start_forwarder_threads(
        args.dest_ip_ports,
        args.src_bind_port,
        args.num_threads,
        deduper.clone(),
        metrics.clone(),
        shutdown_receiver.clone(),
        exit.clone(),
        packet_hold_duration,
    );
    thread_handles.extend(forwarder_hdls);

    let metrics_hdl = forwarder::start_forwarder_accessory_thread(
        deduper,
        metrics.clone(),
        args.metrics_report_interval_ms,
        shutdown_receiver,
        exit,
    );
    thread_handles.push(metrics_hdl);

    info!(
        "Shred-Dos started, listening on {}:{}/udp.",
        args.src_bind_addr, args.src_bind_port
    );

    for thread in thread_handles {
        thread.join().expect("thread panicked");
    }

    info!(
        "Exiting Shred-Dos, {} received , {} sent successfully, {} failed.",
        metrics.agg_received_cumulative.load(Ordering::Relaxed),
        metrics
            .agg_success_forward_cumulative
            .load(Ordering::Relaxed),
        metrics.agg_fail_forward_cumulative.load(Ordering::Relaxed),
    );
    Ok(())
}
