use std::collections::hash_map::Entry as HashEntry;
use std::io::{self, Read, Write};
use std::{mem, thread};
use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// use prelude::*;
use servers2::{spsc, ServerLog};
use hash::{HashMap, FxHasher};
use socket_addr::Ipv4SocketAddr;

use mio;
use mio::tcp::*;

use self::worker::{Worker, WorkerToDist, DistToWorker, ToLog};

use evmap;

mod worker;
mod per_socket;

//Dist tokens
const ACCEPT: mio::Token = mio::Token(0);
const FROM_WORKERS: mio::Token = mio::Token(1);
const DIST_FROM_LOG: mio::Token = mio::Token(2);

//Worker tokens
const FROM_DIST: mio::Token = mio::Token(0);
const FROM_LOG: mio::Token = mio::Token(1);
// we don't really need to special case this;
// all writes are bascially the same,
// but it's convenient for all workers to be in the same token space
const UPSTREAM: mio::Token = mio::Token(2);
const DOWNSTREAM: mio::Token = mio::Token(3);

// It's convenient to share a single token-space among all workers so any worker
// can determine who is responsible for a client
const FIRST_CLIENT_TOKEN: mio::Token = mio::Token(10);

const NUMBER_READ_BUFFERS: usize = 15;

type WorkerNum = usize;

pub fn run(
    acceptor: TcpListener,
    this_server_num: u32,
    total_chain_servers: u32,
    num_workers: usize,
    ready: &AtomicUsize,
) -> ! {
    run_with_replication(acceptor, this_server_num, total_chain_servers, None, None, num_workers, ready)
}

pub fn run_with_replication(
    acceptor: TcpListener,
    this_server_num: u32,
    total_chain_servers: u32,
    prev_server: Option<SocketAddr>,
    next_server: Option<IpAddr>,
    num_workers: usize,
    ready: &AtomicUsize,
) -> ! {
    use std::cmp::max;

    //let (dist_to_workers, recv_from_dist) = spmc::channel();
    //let (log_to_workers, recv_from_log) = spmc::channel();
    //TODO or sync channel?
    let (workers_to_log, recv_from_workers) = mpsc::channel();
    let (workers_to_dist, dist_from_workers) = mio::channel::channel();
    if num_workers == 0 {
        warn!("SERVER {} started with 0 workers.", this_server_num);
    }

    let is_unreplicated = prev_server.is_none() && next_server.is_none();
    if is_unreplicated {
        warn!("SERVER {} started without replication.", this_server_num);
    }
    else {
        trace!("SERVER {} prev: {:?}, next: {:?}.", this_server_num, prev_server, next_server);
    }

    let num_workers = max(num_workers, 1);

    let poll = mio::Poll::new().unwrap();
    poll.register(&acceptor,
        ACCEPT,
        mio::Ready::readable(),
        mio::PollOpt::level()
    ).expect("cannot start server poll");
    let mut events = mio::Events::with_capacity(1023);

    //let next_server_ip: Option<_> = Some(panic!());
    //let prev_server_ip: Option<_> = Some(panic!());
    let next_server_ip: Option<_> = next_server;
    let prev_server_ip: Option<_> = prev_server;
    let mut downstream_admin_socket = None;
    let mut upstream_admin_socket = None;
    let mut other_sockets = Vec::new();
    match acceptor.accept() {
        Err(e) => trace!("error {}", e),
        Ok((socket, addr)) => if Some(addr.ip()) != next_server_ip {
            trace!("SERVER got other connection {:?}", addr);
            other_sockets.push((socket, addr))
        } else {
            trace!("SERVER {} connected downstream.", this_server_num);
            let _ = socket.set_keepalive_ms(Some(1000));
            let _ = socket.set_nodelay(true);
            downstream_admin_socket = Some(socket)
        }
    }
    while next_server_ip.is_some() && downstream_admin_socket.is_none() {
        trace!("SERVER {} waiting for downstream {:?}.", this_server_num, next_server);
        let _ = poll.poll(&mut events, None);
        for event in events.iter() {
            match event.token() {
                ACCEPT => {
                    match acceptor.accept() {
                        Err(e) => trace!("error {}", e),
                        Ok((socket, addr)) => if Some(addr.ip()) != next_server_ip {
                            trace!("SERVER got other connection {:?}", addr);
                            other_sockets.push((socket, addr))
                        } else {
                            trace!("SERVER {} connected downstream.", this_server_num);
                            let _ = socket.set_keepalive_ms(Some(1000));
                            let _ = socket.set_nodelay(true);
                            downstream_admin_socket = Some(socket)
                        }
                    }
                }
                _ => unreachable!()
            }
        }
    }

    if let Some(ref ip) = prev_server_ip {
        trace!("SERVER {} waiting for upstream {:?}.", this_server_num, prev_server_ip);
        while upstream_admin_socket.is_none() {
            if let Ok(socket) = TcpStream::connect(ip) {
                trace!("SERVER {} connected upstream on {:?}.",
                    this_server_num, socket.local_addr().unwrap());
                let _ = socket.set_nodelay(true);
                upstream_admin_socket = Some(socket)
            } else {
                //thread::yield_now()
                thread::sleep(Duration::from_millis(1));
            }
        }
        trace!("SERVER {} connected upstream.", this_server_num);
    }

    let num_downstream = negotiate_num_downstreams(&mut downstream_admin_socket, num_workers as u16);
    let num_upstream = negotiate_num_upstreams(&mut upstream_admin_socket, num_workers as u16, prev_server_ip);
    let mut downstream = Vec::with_capacity(num_downstream);
    let mut upstream = Vec::with_capacity(num_upstream);
    while downstream.len() + 1 < num_downstream {
        let _ = poll.poll(&mut events, None);
        for event in events.iter() {
            match event.token() {
                ACCEPT => {
                    match acceptor.accept() {
                        Err(e) => trace!("error {}", e),
                        Ok((socket, addr)) => if Some(addr.ip()) == next_server_ip {
                            trace!("SERVER {} add downstream.", this_server_num);
                            let _ = socket.set_keepalive_ms(Some(1000));
                            let _ = socket.set_nodelay(true);
                            downstream.push(socket)
                        } else {
                            trace!("SERVER got other connection {:?}", addr);
                            let _ = socket.set_keepalive_ms(Some(1000));
                            let _ = socket.set_nodelay(true);
                            other_sockets.push((socket, addr))
                        }
                    }
                }
                _ => unreachable!()
            }
        }
    }

    if let Some(ref ip) = prev_server_ip {
        for _ in 1..num_upstream {
            let up = TcpStream::connect(ip).expect("cannot connect upstream");
            let _ = up.set_keepalive_ms(Some(1000));
            let _ = up.set_nodelay(true);
            upstream.push(up)
        }
    }

    downstream_admin_socket.take().map(|s| downstream.push(s));
    upstream_admin_socket.take().map(|s| upstream.push(s));
    assert_eq!(downstream.len(), num_downstream);
    assert_eq!(upstream.len(), num_upstream);
    //let (log_to_dist, dist_from_log) = spmc::channel();

    trace!("SERVER {} {} up, {} down.", this_server_num, num_upstream, num_downstream);
    trace!("SERVER {} starting {} workers.", this_server_num, num_workers);
    let mut log_to_workers: Vec<_> = Vec::with_capacity(num_workers);
    let mut dist_to_workers: Vec<_> = Vec::with_capacity(num_workers);
    let (log_reader, log_writer) = evmap::new();
    for n in 0..num_workers {
        //let from_dist = recv_from_dist.clone();
        let to_dist   = workers_to_dist.clone();
        //let from_log  = recv_from_log.clone();
        let to_log = workers_to_log.clone();
        let (to_worker, from_log) = spsc::channel();
        let (dist_to_worker, from_dist) = spsc::channel();
        let upstream = upstream.pop();
        let downstream = downstream.pop();
        let log_reader = log_reader.clone();
        thread::spawn(move ||
            Worker::new(
                from_dist,
                to_dist,
                from_log,
                to_log,
                log_reader,
                upstream,
                downstream,
                num_downstream,
                num_workers,
                is_unreplicated,
                n,
            ).run()
        );
        log_to_workers.push(to_worker);
        dist_to_workers.push(dist_to_worker);
    }
    assert_eq!(dist_to_workers.len(), num_workers);

    //log_to_workers.push(log_to_dist);

    // poll.register(
        // &dist_from_log,
        // DIST_FROM_LOG,
        // mio::Ready::readable(),
        // mio::PollOpt::level()
    // ).expect("cannot pol from log on dist");
    thread::spawn(move || {
        let mut log = ServerLog::new(
            this_server_num, total_chain_servers, log_to_workers, log_writer
        );
        #[cfg(not(feature = "print_stats"))]
        for to_log in recv_from_workers.iter() {
            match to_log {
                ToLog::New(buffer, storage, st) => log.handle_op(buffer, storage, st),
                ToLog::Replication(tr, st) => log.handle_replication(tr, st),
            }
        }
        #[cfg(feature = "print_stats")]
        loop {
            use std::sync::mpsc::RecvTimeoutError;
            let msg = recv_from_workers.recv_timeout(Duration::from_secs(10));
            match msg {
                Ok(ToLog::New(buffer, storage, st)) => log.handle_op(buffer, storage, st),
                Ok(ToLog::Replication(tr, st)) => log.handle_replication(tr, st),
                Err(RecvTimeoutError::Timeout) => log.print_stats(),
                Err(RecvTimeoutError::Disconnected) => panic!("log disconnected"),
            }
        }
    });

    poll.register(&dist_from_workers,
        FROM_WORKERS,
        mio::Ready::readable(),
        mio::PollOpt::level()
    ).unwrap();
    ready.fetch_add(1, Ordering::SeqCst);
    //let mut receivers: HashMap<_, _> = Default::default();
    //FIXME should be a single writer hashmap
    let mut worker_for_client: HashMap<_, _> = Default::default();
    let mut next_token = FIRST_CLIENT_TOKEN;
    //let mut buffer_cache = VecDeque::new();
    let mut next_worker = 0usize;

    for (socket, addr) in other_sockets {
        let tok = get_next_token(&mut next_token);
        let worker = if is_unreplicated {
            let worker = next_worker;
            next_worker = next_worker.wrapping_add(1);
            if next_worker >= dist_to_workers.len() {
                next_worker = 0;
            }
            worker
        } else {
            worker_for_ip(addr, num_workers as u64)
        };
        dist_to_workers[worker].send(DistToWorker::NewClient(tok, socket));
        worker_for_client.insert(
            Ipv4SocketAddr::from_socket_addr(addr), (worker, tok));
    }

    trace!("SERVER start server loop");
    loop {
        let _ = poll.poll(&mut events, None);
        for event in events.iter() {
            match event.token() {
                ACCEPT => {
                    match acceptor.accept() {
                        Err(e) => trace!("error {}", e),
                        Ok((socket, addr)) => {
                            let _ = socket.set_keepalive_ms(Some(1000));
                            let _ = socket.set_nodelay(true);
                            //TODO oveflow
                            let tok = get_next_token(&mut next_token);
                            /*poll.register(
                                &socket,
                                tok,
                                mio::Ready::readable(),
                                mio::PollOpt::edge() | mio::PollOpt::oneshot(),
                            );
                            receivers.insert(tok, Some(socket));*/
                            let worker = if is_unreplicated {
                                let worker = next_worker;
                                next_worker = next_worker.wrapping_add(1);
                                if next_worker >= dist_to_workers.len() {
                                    next_worker = 0;
                                }
                                worker
                            } else {
                                worker_for_ip(addr, num_workers as u64)
                            };
                            trace!("SERVER accepting client @ {:?} => {:?}",
                                addr, (worker, tok));
                            dist_to_workers[worker].send(DistToWorker::NewClient(tok, socket));
                            worker_for_client.insert(
                                Ipv4SocketAddr::from_socket_addr(addr), (worker, tok));
                            //FIXME tell other workers
                        }
                    }
                }
                FROM_WORKERS => {
                    trace!("SERVER dist getting finished work");
                    let packet = dist_from_workers.try_recv();
                    if let Ok(to_worker) = packet {
                        let (worker, token, buffer, addr, storage_loc) = match to_worker {
                            WorkerToDist::Downstream(worker, addr, buffer, storage_loc) => {
                                trace!("DIST {} downstream worker for {} is {}.",
                                    this_server_num, addr, worker);
                                (worker, DOWNSTREAM, buffer, addr, storage_loc)
                            },

                            WorkerToDist::DownstreamB(worker, addr, buffer, storage_loc) => {
                                trace!("DIST {} downstream worker for {} is {}.",
                                    this_server_num, addr, worker);
                                let sent = dist_to_workers.get_mut(worker)
                                    .map(|s| {
                                        s.send(DistToWorker::ToClientB(
                                            DOWNSTREAM, buffer, addr, storage_loc));
                                        true
                                }).unwrap_or(false);
                                if !sent {
                                    panic!("No downstream for {:?} in {:?}",
                                        worker, dist_to_workers.len())
                                }
                                continue
                            },

                            WorkerToDist::ToClient(addr, buffer) => {
                                trace!("DIST {} looking for worker for {}.",
                                    this_server_num, addr);
                                //FIXME this is racey, if we don't know who gets the message it fails
                                let (worker, token) = worker_for_client[&addr].clone();
                                (worker, token, buffer, addr, 0)
                            }

                            WorkerToDist::ToClientB(addr, buffer) => {
                                trace!("DIST {} looking for worker for {}.",
                                    this_server_num, addr);
                                //FIXME this is racey, if we don't know who gets the message it fails
                                let (worker, token) = worker_for_client.get(&addr)
                                    .cloned().unwrap_or_else(||
                                        panic!(
                                            "No worker found for {:?} in {:?}",
                                            addr, worker_for_client,
                                        )
                                );

                                dist_to_workers[worker].send(
                                    DistToWorker::ToClientB(token, buffer, addr, 0)
                                );
                                continue
                            }
                        };
                        dist_to_workers[worker].send(
                            DistToWorker::ToClient(token, buffer, addr, storage_loc));
                        continue

                    }
                    /*while let Ok((buffer, socket, tok)) = dist_from_workers.try_recv() {
                        trace!("SERVER dist got {:?}", tok);
                        //buffer_cache.push_back(buffer);
                        poll.reregister(
                            &socket,
                            tok,
                            mio::Ready::readable(),
                            mio::PollOpt::edge() | mio::PollOpt::oneshot(),
                        );
                        *receivers.get_mut(&tok).unwrap() = Some(socket)
                    }*/
                },
                DIST_FROM_LOG => {
                    //unreachable!()
                    //FIXME handle completing work on original thread, only do send on DOWNSTREAM
                    /*let packet = dist_from_log.try_recv();
                    if let Some(mut to_worker) = packet {
                        let (_, _, addr) = to_worker.get_associated_data();
                        trace!("DIST {} looking for worker for {}.", this_server_num, addr);
                        let (worker, token) = worker_for_client[&addr].clone();
                        to_worker.edit_associated_data(|t| t.1 = token);
                        dist_to_workers[worker].send(DistToWorker::ToClient(to_worker))
                    }*/
                },
                _recv_tok => {
                    //unreachable!()
                    /*let recv = receivers.get_mut(&recv_tok).unwrap();
                    let recv = mem::replace(recv, None);
                    match recv {
                        None => trace!("spurious wakeup for {:?}", recv_tok),
                        Some(socket) => {
                            trace!("SERVER need to recv from {:?}", recv_tok);
                            //TODO should be min size ?
                            let buffer =
                                buffer_cache.pop_back().unwrap_or(Buffer::empty());
                            //dist_to_workers.send((buffer, socket, recv_tok))
                            dist_to_workers[next_worker].send((buffer, socket, recv_tok));
                            next_worker = next_worker.wrapping_add(1);
                            if next_worker >= dist_to_workers.len() {
                                next_worker = 0;
                            }
                        }
                    }*/
                }
            }
        }
    }
}

fn negotiate_num_downstreams(socket: &mut Option<TcpStream>, num_workers: u16) -> usize {
    use std::cmp::min;
    if let Some(ref mut socket) = socket.as_mut() {
        let mut num_other_threads = [0u8; 2];
        blocking_read(socket, &mut num_other_threads).expect("downstream failed");
        let num_other_threads = unsafe { mem::transmute(num_other_threads) };
        let to_write: [u8; 2] = unsafe { mem::transmute(num_workers) };
        blocking_write(socket, &to_write).expect("downstream failed");
        trace!("SERVER down workers: {}, other's workers {}.", num_workers, num_other_threads);
        min(num_other_threads, num_workers) as usize
    }
    else {
        trace!("SERVER no need to negotiate downstream.");
        0
    }
}

fn negotiate_num_upstreams(
    socket: &mut Option<TcpStream>,
    num_workers: u16,
    remote_addr: Option<SocketAddr>
) -> usize {
    use std::cmp::min;
    if let Some(ref mut socket) = socket.as_mut() {
        let remote_addr = remote_addr.unwrap();
        let to_write: [u8; 2] = unsafe { mem::transmute(num_workers) };
        trace!("will req {:?}", to_write);
        let mut refusals = 0;
        'write: loop {
            let r = blocking_write(socket, &to_write);
            match r {
                Err(ref e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    if refusals >= 60000 { panic!("write fail {:?}", e) }
                    refusals += 1;
                    trace!("upstream refused reconnect attempt {}", refusals);
                    thread::sleep(Duration::from_millis(1));
                    **socket = TcpStream::connect(&remote_addr).unwrap();
                    let _ = socket.set_keepalive_ms(Some(1000));
                    let _ = socket.set_nodelay(true);
                }
                Err(ref e) if e.kind() == io::ErrorKind::NotConnected => {
                    if refusals >= 60000 { panic!("write fail {:?}", e) }
                    refusals += 1;
                    trace!("upstream connection not ready {}", refusals);
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("write fail {:?}", e),
                Ok(..) => break 'write,
            }
        }
        trace!("req {:?}", to_write);
        let mut num_other_threads = [0u8; 2];
        blocking_read(socket, &mut num_other_threads).expect("upstream failed");
        trace!("other {:?}", to_write);
        let num_other_threads = unsafe { mem::transmute(num_other_threads) };
        trace!("SERVER up workers: {}, other's workers {}.", num_workers, num_other_threads);
        min(num_other_threads, num_workers) as usize
    }
    else {
        trace!("SERVER no need to negotiate upstream.");
        0
    }
}

fn blocking_read<R: Read>(r: &mut R, mut buffer: &mut [u8]) -> io::Result<()> {
    //like Read::read_exact but doesn't die on WouldBlock
    'recv: while !buffer.is_empty() {
        match r.read(buffer) {
            Ok(i) => { let tmp = buffer; buffer = &mut tmp[i..]; }
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                    thread::yield_now();
                    continue 'recv
                },
                _ => { return Err(e) }
            }
        }
    }
    if !buffer.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof,
            "failed to fill whole buffer"))
    }
    else {
        return Ok(())
    }
}

fn blocking_write<W: Write>(w: &mut W, mut buffer: &[u8]) -> io::Result<()> {
    //like Write::write_all but doesn't die on WouldBlock
    'recv: while !buffer.is_empty() {
        match w.write(buffer) {
            Ok(i) => { let tmp = buffer; buffer = &tmp[i..]; }
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                    thread::yield_now();
                    continue 'recv
                },
                _ => { return Err(e) }
            }
        }
    }
    if !buffer.is_empty() {
        return Err(io::Error::new(io::ErrorKind::WriteZero,
            "failed to fill whole buffer"))
    }
    else {
        return Ok(())
    }
}

#[cfg(False)]
mod tests {
    extern crate env_logger;

    use socket_addr::Ipv4SocketAddr;

    use buffer::Buffer;

    use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use packets::{OrderIndex, EntryFlag, EntryContents, Uuid};
    use packets::SingletonBuilder as Data;

    /*pub fn run(
        acceptor: TcpListener,
        this_server_num: u32,
        total_chain_servers: u32,
        num_workers: usize,
        ready: &AtomicUsize,
    ) -> ! {*/

    #[allow(non_upper_case_globals)]
    const basic_addr: &'static [&'static str] = &["0.0.0.0:13490"];
    #[allow(non_upper_case_globals)]
    const replicas_addr: &'static [&'static str] = &["0.0.0.0:13491", "0.0.0.0:13492"];
    static BASIC_SERVER_READY: AtomicUsize = ATOMIC_USIZE_INIT;
    static REPLICAS_READY: AtomicUsize = ATOMIC_USIZE_INIT;

    #[test]
    fn test_write() {
        let _ = env_logger::init();
        trace!("TCP test write");
        start_servers(basic_addr, &BASIC_SERVER_READY);
        trace!("TCP test write start");
        let mut stream = TcpStream::connect(&"127.0.0.1:13490").unwrap();
        let _ = stream.set_nodelay(true);
        let mut buffer = Buffer::empty();
        Data(&12i32, &[]).fill_entry(&mut buffer);
        buffer.contents_mut().locs_mut()[0] = OrderIndex(1.into(), 0.into());
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut stream);
        assert!(buffer.contents().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(1.into(), 1.into()));
        assert_eq!(buffer.contents().into_singleton_builder(), Data(&12i32, &[]));
    }

    #[test]
    fn test_write_read() {
        let _ = env_logger::init();
        trace!("TCP test write_read");
        start_servers(basic_addr, &BASIC_SERVER_READY);
        trace!("TCP test write_read start");
        let mut stream = TcpStream::connect(&"127.0.0.1:13490").unwrap();
        let _ = stream.set_nodelay(true);
        let mut buffer = Buffer::empty();
        Data(&92u64, &[]).fill_entry(&mut buffer);
        {
            buffer.contents_mut().locs_mut()[0] = OrderIndex(2.into(), 0.into());
        }
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut stream);
        assert!(buffer.contents().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(2.into(), 1.into()));
        assert_eq!(buffer.contents().into_singleton_builder(), Data(&92u64, &[]));
        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(0.into(), 0.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        buffer.contents_mut().locs_mut()[0] = OrderIndex(2.into(), 1.into());
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut stream);
        assert!(buffer.contents().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(2.into(), 1.into()));
        assert_eq!(buffer.contents().into_singleton_builder(), Data(&92u64, &[]));
    }

    #[test]
    fn test_replicated_write() {
        let _ = env_logger::init();
        trace!("TCP test replicated write");
        start_servers(replicas_addr, &REPLICAS_READY);
        trace!("TCP test replicated write start");
        let mut write_stream = TcpStream::connect(&"127.0.0.1:13491").unwrap();
        let mut read_stream = TcpStream::connect(&"127.0.0.1:13492").unwrap();
        let read_addr = Ipv4SocketAddr::from_socket_addr(read_stream.local_addr().unwrap());
        let mut buffer = Buffer::empty();
        Data(&12i32, &[]).fill_entry(&mut buffer);
        buffer.contents_mut().locs_mut()[0] = OrderIndex(1.into(), 0.into());
        trace!("sending write");
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        trace!("finished sending write, waiting for ack");
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut read_stream);
        trace!("finished waiting for ack");
        assert!(buffer.contents().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(1.into(), 1.into()));
        assert_eq!(buffer.contents().into_singleton_builder(), Data(&12i32, &[]));
    }

    #[test]
    fn test_replicated_write_read() {
        let _ = env_logger::init();
        trace!("TCP test replicated write/read");
        start_servers(replicas_addr, &REPLICAS_READY);
        trace!("TCP test replicated write/read start");
        let mut write_stream = TcpStream::connect(&"127.0.0.1:13491").unwrap();
        let mut read_stream = TcpStream::connect(&"127.0.0.1:13492").unwrap();
        let read_addr = Ipv4SocketAddr::from_socket_addr(read_stream.local_addr().unwrap());
        let mut buffer = Buffer::empty();
        Data(&92u64, &[]).fill_entry(&mut buffer);;
        buffer.contents_mut().locs_mut()[0] = OrderIndex(2.into(), 0.into());
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut read_stream);
        assert!(buffer.entry().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(2.into(), 1.into()));
        assert_eq!(buffer.contents().into_singleton_builder(), Data(&92u64, &[]));
        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(0.into(), 0.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        buffer.ensure_len();
        buffer.contents_mut().locs_mut()[0] = OrderIndex(2.into(), 1.into());
        read_stream.write_all(buffer.entry_slice()).unwrap();
        read_stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut read_stream);
        assert!(buffer.entry().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(2.into(), 1.into()));
        assert_eq!(buffer.contents().into_singleton_builder(), Data(&92u64, &[]));
    }

    #[test]
    fn test_skeens_write() {
        let _ = env_logger::init();
        trace!("TCP test write");
        start_servers(basic_addr, &BASIC_SERVER_READY);
        trace!("TCP test write start");
        let mut stream = TcpStream::connect(&"127.0.0.1:13490").unwrap();
        let _ = stream.set_nodelay(true);
        let mut buffer = Buffer::empty();
        let id = Uuid::new_v4();
        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock),
            lock: &0,
            locs: &[OrderIndex(3.into(), 0.into()), OrderIndex(4.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut stream);
        assert!(buffer.contents().flag().contains(EntryFlag::Skeens1Queued));
        assert_eq!(buffer.contents(), EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::Skeens1Queued | EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            data_bytes: &3, //TODO make 0
            lock: &0,
            locs: &[OrderIndex(3.into(), 1.into()), OrderIndex(4.into(), 1.into())],
            deps: &[],
        });
        let max_timestamp = buffer.contents().locs().iter()
        .fold(0, |max_ts, &OrderIndex(_, i)|
            ::std::cmp::max(max_ts, u32::from(i) as u64)
        );
        assert!(max_timestamp > 0);

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::Unlock),
            data_bytes: &0,
            lock: &max_timestamp,
            locs: &[OrderIndex(3.into(), 0.into()), OrderIndex(4.into(), 0.into())],
            deps: &[],
        });
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();

        /*buffer.clear_data();

        recv_packet(&mut buffer, &mut stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(3.into(), 1.into()), OrderIndex(4.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });*/

        buffer.clear_data();

        recv_packet(&mut buffer, &mut stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(3.into(), 1.into()), OrderIndex(4.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
    }

    #[test]
    fn test_skeens_write_read() {
        let _ = env_logger::init();
        trace!("TCP test write");
        start_servers(basic_addr, &BASIC_SERVER_READY);
        trace!("TCP test write start");
        let mut stream = TcpStream::connect(&"127.0.0.1:13490").unwrap();
        let _ = stream.set_nodelay(true);
        let mut buffer = Buffer::empty();
        let id = Uuid::new_v4();
        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock),
            lock: &0,
            locs: &[OrderIndex(5.into(), 0.into()), OrderIndex(6.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut stream);
        assert!(buffer.contents().flag().contains(EntryFlag::Skeens1Queued));
        assert_eq!(buffer.contents(), EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::Skeens1Queued | EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            data_bytes: &3, //TODO make 0
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
        });
        let max_timestamp = buffer.contents().locs().iter()
        .fold(0, |max_ts, &OrderIndex(_, i)|
            ::std::cmp::max(max_ts, u32::from(i) as u64)
        );
        assert!(max_timestamp > 0);

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::Unlock),
            data_bytes: &0,
            lock: &max_timestamp,
            locs: &[OrderIndex(5.into(), 0.into()), OrderIndex(6.into(), 0.into())],
            deps: &[],
        });
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();

        /*buffer.clear_data();

        recv_packet(&mut buffer, &mut stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });*/

        buffer.clear_data();

        recv_packet(&mut buffer, &mut stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });


        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(5.into(), 1.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(6.into(), 1.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
    }

    #[test]
    fn test_replicated_skeens_write() {
        let _ = env_logger::init();
        trace!("TCP test replicated write");
        start_servers(replicas_addr, &REPLICAS_READY);
        trace!("TCP test replicated write start");
        let mut write_stream = TcpStream::connect(&"127.0.0.1:13491").unwrap();
        let mut read_stream = TcpStream::connect(&"127.0.0.1:13492").unwrap();
        let read_addr = Ipv4SocketAddr::from_socket_addr(read_stream.local_addr().unwrap());

        let mut buffer = Buffer::empty();
        let id = Uuid::new_v4();
        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock),
            lock: &0,
            locs: &[OrderIndex(3.into(), 0.into()), OrderIndex(4.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::Skeens1Queued | EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            data_bytes: &3, //TODO make 0
            lock: &0,
            locs: &[OrderIndex(3.into(), 1.into()), OrderIndex(4.into(), 1.into())],
            deps: &[],
        });
        let max_timestamp = buffer.contents().locs().iter()
        .fold(0, |max_ts, &OrderIndex(_, i)|
            ::std::cmp::max(max_ts, u32::from(i) as u64)
        );
        assert!(max_timestamp > 0);

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::Unlock),
            data_bytes: &0,
            lock: &max_timestamp,
            locs: &[OrderIndex(3.into(), 0.into()), OrderIndex(4.into(), 0.into())],
            deps: &[],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();

        /*buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(3.into(), 1.into()), OrderIndex(4.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });*/

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(3.into(), 1.into()), OrderIndex(4.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
    }

    #[test]
    fn test_replicated_skeens_write_read() {
        let _ = env_logger::init();
        trace!("TCP test replicated write");
        start_servers(replicas_addr, &REPLICAS_READY);
        trace!("TCP test replicated write start");
        let mut write_stream = TcpStream::connect(&"127.0.0.1:13491").unwrap();
        let mut read_stream = TcpStream::connect(&"127.0.0.1:13492").unwrap();
        let read_addr = Ipv4SocketAddr::from_socket_addr(read_stream.local_addr().unwrap());

        let mut buffer = Buffer::empty();
        let id = Uuid::new_v4();
        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock),
            lock: &0,
            locs: &[OrderIndex(5.into(), 0.into()), OrderIndex(6.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::Skeens1Queued | EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            data_bytes: &3, //TODO make 0
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
        });
        let max_timestamp = buffer.contents().locs().iter()
        .fold(0, |max_ts, &OrderIndex(_, i)|
            ::std::cmp::max(max_ts, u32::from(i) as u64)
        );
        assert!(max_timestamp > 0);

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::Unlock),
            data_bytes: &0,
            lock: &max_timestamp,
            locs: &[OrderIndex(5.into(), 0.into()), OrderIndex(6.into(), 0.into())],
            deps: &[],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();

        /*buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });*/

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        ///

        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(5.into(), 1.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        read_stream.write_all(buffer.entry_slice()).unwrap();
        read_stream.write_all(&[0; 6]).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(6.into(), 1.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        read_stream.write_all(buffer.entry_slice()).unwrap();
        read_stream.write_all(&[0; 6]).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(5.into(), 1.into()), OrderIndex(6.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
    }

    #[test]
    fn test_replicated_skeens_single_write_read() {
        let _ = env_logger::init();
        trace!("TCP test replicated s write/r");
        start_servers(replicas_addr, &REPLICAS_READY);
        trace!("TCP test replicated write start");
        let mut write_stream = TcpStream::connect(&"127.0.0.1:13491").unwrap();
        let mut read_stream = TcpStream::connect(&"127.0.0.1:13492").unwrap();
        let read_addr = Ipv4SocketAddr::from_socket_addr(read_stream.local_addr().unwrap());

        let mut buffer = Buffer::empty();
        let id = Uuid::new_v4();
        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock),
            lock: &0,
            locs: &[OrderIndex(7.into(), 0.into()), OrderIndex(8.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::Skeens1Queued | EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            data_bytes: &3, //TODO make 0
            lock: &0,
            locs: &[OrderIndex(7.into(), 1.into()), OrderIndex(8.into(), 1.into())],
            deps: &[],
        });
        let max_timestamp = buffer.contents().locs().iter()
        .fold(0, |max_ts, &OrderIndex(_, i)|
            ::std::cmp::max(max_ts, u32::from(i) as u64)
        );
        assert!(max_timestamp > 0);

        buffer.clear_data();

        trace!("test_replicated_skeens_single_write_read finished phase 1");

        let id2 = Uuid::new_v4();

        buffer.fill_from_entry_contents(EntryContents::Single {
            id: &id2,
            flags: &EntryFlag::Nothing,
            loc: &OrderIndex(7.into(), 0.into()),
            deps: &[],
            data: &[1, 1, 1, 1, 2],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::Unlock),
            data_bytes: &0,
            lock: &max_timestamp,
            locs: &[OrderIndex(7.into(), 0.into()), OrderIndex(8.into(), 0.into())],
            deps: &[],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();

        buffer.clear_data();

        /*trace!("test_replicated_skeens_single_write_read finished phase 2");

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(7.into(), 1.into()), OrderIndex(8.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        buffer.clear_data();*/

        trace!("test_replicated_skeens_single_write_read finished phase 3");

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Single {
            id: &id2,
            flags: &EntryFlag::ReadSuccess,
            loc: &OrderIndex(7.into(), 2.into()),
            deps: &[],
            data: &[1, 1, 1, 1, 2],
        });

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(7.into(), 1.into()), OrderIndex(8.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        trace!("test_replicated_skeens_single_write_read finished phase 4");

        ///

        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::ReadSuccess,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(7.into(), 1.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        read_stream.write_all(buffer.entry_slice()).unwrap();
        read_stream.write_all(&[0; 6]).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(7.into(), 1.into()), OrderIndex(8.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(8.into(), 1.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        read_stream.write_all(buffer.entry_slice()).unwrap();
        read_stream.write_all(&[0; 6]).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(7.into(), 1.into()), OrderIndex(8.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
    }

    #[test]
    fn test_replicated_skeens_single_server_multi() {
        let _ = env_logger::init();
        trace!("TCP test replicated ssm");
        start_servers(replicas_addr, &REPLICAS_READY);
        trace!("TCP test replicated write ssm start");
        let mut write_stream = TcpStream::connect(&"127.0.0.1:13491").unwrap();
        let mut read_stream = TcpStream::connect(&"127.0.0.1:13492").unwrap();
        let read_addr = Ipv4SocketAddr::from_socket_addr(read_stream.local_addr().unwrap());

        let mut buffer = Buffer::empty();
        let id = Uuid::new_v4();
        let id2 = Uuid::new_v4();;
        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock),
            lock: &0,
            locs: &[OrderIndex(9.into(), 0.into()), OrderIndex(10.into(), 0.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        buffer.clear_data();
        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::Skeens1Queued | EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            data_bytes: &3, //TODO make 0
            lock: &0,
            locs: &[OrderIndex(9.into(), 1.into()), OrderIndex(10.into(), 1.into())],
            deps: &[],
        });
        let max_timestamp = buffer.contents().locs().iter()
        .fold(0, |max_ts, &OrderIndex(_, i)|
            ::std::cmp::max(max_ts, u32::from(i) as u64)
        );
        assert!(max_timestamp > 0);
        buffer.clear_data();

        buffer.fill_from_entry_contents(EntryContents::Multi{
            id: &id2,
            flags: &EntryFlag::Nothing,
            lock: &0,
            locs: &[OrderIndex(9.into(), 0.into()), OrderIndex(10.into(), 0.into())],
            deps: &[],
            data: &[123, 01, 255, 11],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();
        buffer.clear_data();


        buffer.fill_from_entry_contents(EntryContents::Senti{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::Unlock),
            data_bytes: &0,
            lock: &max_timestamp,
            locs: &[OrderIndex(9.into(), 0.into()), OrderIndex(10.into(), 0.into())],
            deps: &[],
        });
        write_stream.write_all(buffer.entry_slice()).unwrap();
        write_stream.write_all(read_addr.bytes()).unwrap();

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id,
            flags: &(EntryFlag::NewMultiPut | EntryFlag::TakeLock | EntryFlag::ReadSuccess),
            lock: &0,
            locs: &[OrderIndex(9.into(), 1.into()), OrderIndex(10.into(), 1.into())],
            deps: &[],
            data: &[94, 49, 0xff],
        });

        buffer.clear_data();

        recv_packet(&mut buffer, &mut read_stream);
        assert_eq!(buffer.contents(), EntryContents::Multi{
            id: &id2,
            flags: &EntryFlag::ReadSuccess,
            lock: &0,
            locs: &[OrderIndex(9.into(), 2.into()), OrderIndex(10.into(), 2.into())],
            deps: &[],
            data: &[123, 01, 255, 11],
        });
    }

    #[test]
    fn test_empty_read() {
        let _ = env_logger::init();
        trace!("TCP test write_read");
        start_servers(basic_addr, &BASIC_SERVER_READY);
        trace!("TCP test write_read start");
        let mut stream = TcpStream::connect(&"127.0.0.1:13490").unwrap();
        let mut buffer = Buffer::empty();
        Data(&(), &[OrderIndex(0.into(), 0.into())]).fill_entry(&mut buffer);
        buffer.fill_from_entry_contents(EntryContents::Read {
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(0.into(), 0.into()),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        });
        buffer.ensure_len();
        buffer.contents_mut().locs_mut()[0] = OrderIndex(0.into(), 1.into());
        stream.write_all(buffer.entry_slice()).unwrap();
        stream.write_all(&[0; 6]).unwrap();
        buffer[..].iter_mut().fold((), |_, i| *i = 0);
        recv_packet(&mut buffer, &mut stream);
        assert!(!buffer.entry().flag().contains(EntryFlag::ReadSuccess));
        assert_eq!(buffer.contents().locs()[0], OrderIndex(0.into(), 1.into()));
        assert_eq!(buffer.contents().horizon(), OrderIndex(0.into(), 0.into()));
    }

    //FIXME add empty read tests

    fn recv_packet(buffer: &mut Buffer, mut stream: &TcpStream) {
        use packets::Packet::WrapErr;
        let mut read = 0;
        loop {
            let to_read = buffer.finished_at(read);
            let size = match to_read {
                Err(WrapErr::NotEnoughBytes(needs)) => needs,
                Err(err) => panic!("{:?}", err),
                Ok(size) if read < size => size,
                Ok(..) => return,
            };
            let r = stream.read(&mut buffer[read..size]);
            match r {
                Ok(i) => read += i,
                Err(e) => panic!("recv error {:?}", e),
            }
        }
    }

    fn start_servers<'a, 'b>(addr_strs: &'a [&'b str], server_ready: &'static AtomicUsize) {
        use std::thread;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        use mio;

        trace!("starting server(s) @ {:?}", addr_strs);

        //static SERVERS_READY: AtomicUsize = ATOMIC_USIZE_INIT;

        if addr_strs.len() == 1 {
            let addr = addr_strs[0].parse().expect("invalid inet address");
            let acceptor = mio::tcp::TcpListener::bind(&addr);
            if let Ok(acceptor) = acceptor {
                thread::spawn(move || {
                    trace!("starting server");
                    ::servers2::tcp::run(acceptor, 0, 1, 1, server_ready)
                });
            }
            else {
                trace!("server already started @ {}", addr_strs[0]);
            }
        }
        else {
            let local_host = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
            for i in 0..addr_strs.len() {
                let prev_server: Option<SocketAddr> =
                    if i > 0 { Some(addr_strs[i-1]) } else { None }
                    .map(|s| s.parse().unwrap());
                let prev_server = prev_server.map(|mut s| {s.set_ip(local_host); s});
                let next_server: Option<SocketAddr> = addr_strs.get(i+1)
                    .map(|s| s.parse().unwrap());
                let next_server = next_server.map(|mut s| {s.set_ip(local_host); s});
                let next_server = next_server.map(|s| s.ip());
                let addr = addr_strs[i].parse().unwrap();
                let acceptor = mio::tcp::TcpListener::bind(&addr);
                if let Ok(acceptor) = acceptor {
                    thread::spawn(move || {
                        trace!("starting replica server");
                        ::servers2::tcp::run_with_replication(acceptor, 0, 1,
                            prev_server, next_server,
                            1, server_ready)
                    });
                }
                else {
                    trace!("server already started @ {}", addr_strs[i]);
                }
            }
        }

        while server_ready.load(Ordering::Acquire) < addr_strs.len() {}
    }
}

fn get_next_token(token: &mut mio::Token) -> mio::Token {
    *token = mio::Token(token.0.checked_add(1).unwrap());
    *token
}

fn worker_for_ip(ip: SocketAddr, num_workers: u64) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher: FxHasher = Default::default();
    ip.hash(&mut hasher);
    (hasher.finish() % num_workers) as usize
}
