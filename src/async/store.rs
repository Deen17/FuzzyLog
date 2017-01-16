use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::mem;
use std::net::SocketAddr;
use std::time::Duration;
use std::rc::Rc;

use packets::*;
use buffer::Buffer;

use hash::HashMap;

use mio;
use mio::tcp::*;
use mio::Token;
use mio::udp::UdpSocket;

pub trait AsyncStoreClient {
    //TODO nocopy?
    fn on_finished_read(&mut self, read_loc: OrderIndex, read_packet: Vec<u8>);
    //TODO what info is needed?
    fn on_finished_write(&mut self, write_id: Uuid, write_locs: Vec<OrderIndex>);

    //TODO fn should_shutdown(&mut self) -> bool { false }
}

pub struct AsyncTcpStore<Socket, C: AsyncStoreClient> {
    servers: Vec<PerServer<Socket>>,
    awake_io: VecDeque<usize>,
    sent_writes: HashMap<Uuid, WriteState>,
    sent_reads: HashMap<OrderIndex, Vec<u8>>,
    num_chain_servers: usize,
    //FIXME change to spsc::Receiver<Buffer?>
    from_client: mio::channel::Receiver<Vec<u8>>,
    client: C,
}

pub struct PerServer<Socket> {
    awaiting_send: VecDeque<WriteState>,
    read_buffer: Buffer,
    stream: Socket,
    bytes_read: usize,
    bytes_sent: usize,
    currently_sending: Option<WriteState>,
    got_new_message: bool,
    //FIXME receiver: SocketAddr,
}

#[derive(Debug)]
enum WriteState {
    SingleServer(Vec<u8>),
    ToLockServer(Vec<u8>, Vec<u8>),
    MultiServer(Rc<RefCell<Vec<u8>>>, Rc<HashMap<usize, u64>>),
    UnlockServer(Rc<RefCell<Vec<u8>>>, u64),
}

struct UdpConnection {
    socket: UdpSocket,
    addr: SocketAddr,
}

pub trait Connected {
    type Connection: mio::Evented;
    fn connection(&self) -> &Self::Connection;
    fn recv_packet(&mut self) -> Result<Option<Buffer>, io::Error>;
    fn send_packet(&mut self, packet: &[u8]) -> bool;
    fn send_read_packet(&mut self, packet: &[u8]) -> bool {
        self.send_packet(packet)
    }
}

//TODO rename to AsyncStore
impl<C> AsyncTcpStore<TcpStream, C>
where C: AsyncStoreClient {

    //TODO should probably move all the poll register stuff too run
    pub fn tcp<I>(lock_server: SocketAddr, chain_servers: I, client: C,
        event_loop: &mut mio::Poll)
    -> Result<(Self, mio::channel::Sender<Vec<u8>>), io::Error>
    where I: IntoIterator<Item=SocketAddr> {
        //TODO assert no duplicates
        let mut servers = try!(chain_servers
            .into_iter()
            .map(PerServer::tcp)
            .collect::<Result<Vec<_>, _>>());
        let num_chain_servers = servers.len();
        let lock_server = try!(PerServer::tcp(lock_server));
        //TODO if let Some(lock_server) = lock_server...
        servers.push(lock_server);
        for (i, server) in servers.iter().enumerate() {
            event_loop.register(server.connection(), Token(i),
                mio::Ready::readable() | mio::Ready::writable() | mio::Ready::error(),
                mio::PollOpt::edge())
                .expect("could not reregister client socket")
        }
        let from_client_token = Token(servers.len());
        let (to_store, from_client) = mio::channel::channel();
        event_loop.register(&from_client,
            from_client_token,
            mio::Ready::readable() | mio::Ready::error(),
            mio::PollOpt::level()
        ).expect("could not reregister client channel");
        Ok((AsyncTcpStore {
            sent_writes: Default::default(),
            sent_reads: Default::default(),
            servers: servers,
            num_chain_servers: num_chain_servers,
            client: client,
            awake_io: Default::default(),
            from_client: from_client,
        }, to_store))
    }
}

impl<C> AsyncTcpStore<UdpConnection, C>
where C: AsyncStoreClient {
    pub fn udp<I>(lock_server: SocketAddr, chain_servers: I, client: C,
        event_loop: &mut mio::Poll)
    -> Result<(Self, mio::channel::Sender<Vec<u8>>), io::Error>
    where I: IntoIterator<Item=SocketAddr> {
        //TODO assert no duplicates
        let mut servers = try!(chain_servers
            .into_iter()
            //TODO socket per servers is dumb, see if there's a better way to do multiplexing
            .map(PerServer::udp)
            .collect::<Result<Vec<_>, _>>());
        let num_chain_servers = servers.len();
        let lock_server = try!(PerServer::udp(lock_server));
        //TODO if let Some(lock_server) = lock_server...
        servers.push(lock_server);
        for (i, server) in servers.iter().enumerate() {
            event_loop.register(server.connection(), Token(i),
                mio::Ready::readable() | mio::Ready::writable() | mio::Ready::error(),
                mio::PollOpt::edge())
                .expect("could not reregister client socket")
        }
        let from_client_token = Token(servers.len());
        let (to_store, from_client) = mio::channel::channel();
        event_loop.register(&from_client,
            from_client_token,
            mio::Ready::readable() | mio::Ready::error(),
            mio::PollOpt::level()
        ).expect("could not reregister client channel");
        Ok((AsyncTcpStore {
            sent_writes: Default::default(),
            sent_reads: Default::default(),
            awake_io: Default::default(),
            servers: servers,
            num_chain_servers: num_chain_servers,
            client: client,
            from_client: from_client,
        }, to_store))
    }
}

impl PerServer<TcpStream> {
    fn tcp(addr: SocketAddr) -> Result<Self, io::Error> {
        Ok(PerServer {
            awaiting_send: VecDeque::new(),
            read_buffer: Buffer::new(), //TODO cap
            stream: try!(TcpStream::connect(&addr)),
            bytes_read: 0,
            bytes_sent: 0,
            currently_sending: None,
            got_new_message: false,
        })
    }

    fn connection(&self) -> &TcpStream {
        &self.stream
    }
}

impl PerServer<UdpConnection> {
    fn udp(addr: SocketAddr) -> Result<Self, io::Error> {
        use std::os::unix::io::FromRawFd;
        use nix::sys::socket as nix;
        let fd: i32 = try!(nix::socket(
                nix::AddressFamily::Inet,
                nix::SockType::Datagram,
                nix::SOCK_NONBLOCK | nix::SOCK_CLOEXEC,
                0));
        Ok(PerServer {
            awaiting_send: VecDeque::new(),
            read_buffer: Buffer::new(), //TODO cap
            stream: UdpConnection { socket: unsafe { UdpSocket::from_raw_fd(fd) }, addr: addr },
            bytes_read: 0,
            bytes_sent: 0,
            currently_sending: None,
            got_new_message: false,
        })
    }

    fn connection(&self) -> &UdpSocket {
        &self.stream.socket
    }
}

impl<S, C> AsyncTcpStore<S, C>
where PerServer<S>: Connected,
      C: AsyncStoreClient {
    pub fn run(mut self, poll: mio::Poll) -> ! {
        let mut events = mio::Events::with_capacity(1024);
        loop {
            poll.poll(&mut events, None).expect("worker poll failed");
            self.handle_new_events(events.iter());

            'work: loop {
                let ops_before_poll = self.awake_io.len();
                for _ in 0..ops_before_poll {
                    let server = self.awake_io.pop_front().unwrap();
                    self.handle_server_event(server);
                }
                if self.awake_io.is_empty() {
                    break 'work
                }
                poll.poll(&mut events, Some(Duration::new(0, 0))).expect("worker poll failed");
                self.handle_new_events(events.iter());
            }
        }
    }// end fn run

    fn handle_new_events(&mut self, events: mio::EventsIter) {
        for event in events {
            let token = event.token();
            if token.0 >= self.servers.len() {
                self.handle_new_requests_from_client()
            }
            else {
                debug_assert!(token.0 < self.servers.len());
                self.handle_server_event(event.token().0)
            }
        }
    }

    fn handle_new_requests_from_client(&mut self) {
        use std::sync::mpsc::TryRecvError;
        trace!("CLIENT got new req");
        let msg = match self.from_client.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Empty) => return,
            //TODO Err(TryRecvError::Disconnected) => panic!("client disconnected.")
            Err(TryRecvError::Disconnected) => return,
        };
        let new_msg_kind = Entry::<()>::wrap_bytes(&msg).kind.layout();
        match new_msg_kind {
            EntryLayout::Read | EntryLayout::Data => {
                let loc = Entry::<()>::wrap_bytes(&msg).locs()[0].0;
                let s = self.server_for_chain(loc);
                //TODO if writeable write?
                self.add_single_server_send(s, msg);
            }
            EntryLayout::Multiput => {
                if self.is_single_node_append(&msg) {
                    let chain = Entry::<()>::wrap_bytes(&msg).locs()[0].0;
                    let s = self.server_for_chain(chain);
                    self.add_single_server_send(s, msg);
                }
                else {
                    let mut msg = msg;
                    Entry::<()>::wrap_bytes_mut(&mut msg).locs_mut().into_iter()
                        .fold((), |_, &mut OrderIndex(_,ref mut i)| *i = 0.into());
                    self.add_get_lock_nums(msg);
                };
            }
            r @ EntryLayout::Sentinel | r @ EntryLayout::Lock =>
                panic!("Invalid send request {:?}", r),
        }
    } // End fn handle_new_requests_from_client

    fn add_single_server_send(&mut self, server: usize, msg: Vec<u8>) {
        assert_eq!(Entry::<()>::wrap_bytes(&msg).entry_size(), msg.len());
        //let per_server = self.server_for_token_mut(Token(server));
        let per_server = &mut self.servers[server];
        per_server.add_single_server_send(msg);
        if !per_server.got_new_message {
            per_server.got_new_message = true;
            self.awake_io.push_back(server)
        }
    }

    fn add_get_lock_nums(&mut self, mut msg: Vec<u8>) {
        Entry::<()>::wrap_bytes_mut(&mut msg).kind.insert(EntryKind::TakeLock);
        let lock_chains = self.get_lock_server_chains_for(&msg);
        //TODO we shouldn't just alloc this...
        let lock_req = EntryContents::Multiput {
               data: &msg, uuid: &Uuid::new_v4(), columns: &lock_chains, deps: &[],
        }.clone_bytes();
        let lock_server = self.lock_token();
        //let per_server = self.server_for_token_mut(lock_server);
        let per_server = &mut self.servers[lock_server.0];
        per_server.add_get_lock_nums(lock_req, msg);
        if !per_server.got_new_message {
            per_server.got_new_message = true;
            self.awake_io.push_back(lock_server.0)
        }
    }

    fn handle_server_event(&mut self, server: usize) {
        trace!("CLIENT handle server event");
        self.servers[server].got_new_message = false;
        //TODO pass in whether a read or write is ready?
        let mut stay_awake = false;
        let token = Token(server);
        let finished_recv = self.servers[server].recv_packet().expect("cannot recv");
        if let Some(packet) = finished_recv {
            stay_awake = true;
            let kind = packet.entry().kind;
            trace!("CLIENT got a {:?} from {:?}", kind, token);
            if kind.contains(EntryKind::ReadSuccess) {
                let num_chain_servers = self.num_chain_servers;
                self.handle_completion(token, num_chain_servers, &packet)
            }
            //TODO distinguish between locks and empties
            else if kind.layout() == EntryLayout::Read
                && !kind.contains(EntryKind::TakeLock) {
                //A read that found an usused entry still contains useful data
                self.handle_completed_read(token, &packet)
            }
            //TODO use option instead
            else {
                self.handle_redo(Token(server), kind, &packet)
            }
            self.server_for_token_mut(token).read_buffer = packet
        }

        if self.servers[server].needs_to_write() {
            let num_chain_servers = self.num_chain_servers;
            let finished_send = self.servers[server].send_next_packet(token, num_chain_servers);
            if let Some(sent) = finished_send {
                trace!("CLIENT finished a send to {:?}", token);
                stay_awake = true;
                let layout = sent.layout();
                if layout == EntryLayout::Read {
                    let read_loc = sent.read_loc();
                    self.sent_reads.insert(read_loc, sent.take());
                }
                else if !sent.is_unlock() {
                    let id = sent.id();
                    self.sent_writes.insert(id, sent);
                }
            }
        }

        if stay_awake && self.servers[server].got_new_message == false {
            self.awake_io.push_back(server)
        }
    } // end fn handle_server_event

    fn handle_completion(&mut self, token: Token, num_chain_servers: usize, packet: &Buffer) {
        let write_completed = self.handle_completed_write(token, num_chain_servers, packet);
        if write_completed {
            self.handle_completed_read(token, packet);
        }
    }

    //TODO I should probably return the buffer on recv_packet, and pass it into here
    fn handle_completed_write(&mut self, token: Token, num_chain_servers: usize,
        packet: &Buffer) -> bool {
        let id = packet.entry().id;
        trace!("CLIENT handle completed write");
        //TODO for multistage writes check if we need to do more work...
        if let Some(v) = self.sent_writes.remove(&id) {
            match v {
                //if lock
                //    for server in v.servers()
                //         server.send_lock+data
                WriteState::ToLockServer(_, msg) => {
                    trace!("CLIENT finished lock");
                    //TODO should I bother keeping around lockbuf for resends?
                    assert_eq!(token, self.lock_token());
                    let lock_nums = packet.get_lock_nums();
                    self.add_multis(msg, lock_nums);
                    return false
                }
                WriteState::MultiServer(buf, lock_nums) => {
                    assert!(token != self.lock_token());
                    trace!("CLIENT finished multi section");
                    let ready_to_unlock = {
                        let mut b = buf.borrow_mut();
                        let finished_writes = fill_locs(&mut b, packet.entry(),
                            token, num_chain_servers);
                        if finished_writes {
                            //TODO assert!(Rc::get_mut(&mut buf).is_some());
                            //let locs = buf.locs().to_vec();
                            let locs = Entry::<()>::wrap_bytes(&b).locs().to_vec();
                            Some(locs)
                        } else { None }
                    };
                    match ready_to_unlock {
                        Some(locs) => {
                            self.add_unlocks(buf, lock_nums);
                            trace!("CLIENT finished multi at {:?}", locs);
                            //TODO
                            self.client.on_finished_write(id, locs);
                            return true
                        }
                        None => {
                            self.sent_writes.insert(id,
                                WriteState::MultiServer(buf, lock_nums));
                            return false
                        }
                    }
                }
                WriteState::SingleServer(mut buf) => {
                    assert!(token != self.lock_token());
                    trace!("CLIENT finished single server");
                    fill_locs(&mut buf, packet.entry(), token, num_chain_servers);
                    let locs = packet.entry().locs().to_vec();
                    self.client.on_finished_write(id, locs);
                    return true
                }
                WriteState::UnlockServer(..) => panic!("invalid wait state"),
            };

            fn fill_locs(buf: &mut [u8], e: &Entry<()>,
                server: Token, num_chain_servers: usize) -> bool {
                let locs = Entry::<()>::wrap_bytes_mut(buf).locs_mut();
                let mut remaining = locs.len();
                let fill_from = e.locs();
                //trace!("CLIENT filling {:?} from {:?}", locs, fill_from);
                for (i, &loc) in fill_from.into_iter().enumerate() {
                    if locs[i].0 != 0.into()
                        && server_for_chain(loc.0, num_chain_servers) == server.0 {
                        assert!(loc.1 != 0.into(), "zero index for {:?}", loc.0);
                        locs[i] = loc;
                    }
                    if locs[i] == OrderIndex(0.into(), 0.into()) || locs[i].1 != 0.into() {
                        remaining -= 1;
                    }
                }
                remaining == 0
            }
        }
        else {
            trace!("CLIENT no write for {:?}", token);
            return true
        }
    }// end handle_completed_write

    fn handle_completed_read(&mut self, token: Token, packet: &Buffer) {
        trace!("CLIENT handle completed read");
        if token == self.lock_token() { return }
        for &oi in packet.entry().locs() {
            trace!("CLIENT completed read @ {:?}", oi);
            if let Some(mut v) = self.sent_reads.remove(&oi) {
                //TODO validate correct id for failing read
                //TODO num copies?
                v.clear();
                v.extend_from_slice(&packet[..]);
                self.client.on_finished_read(oi, v);
            }
        }
    }

    fn handle_redo(&mut self, token: Token, kind: EntryKind::Kind, packet: &Buffer) {
        let write_state = match kind.layout() {
            EntryLayout::Data | EntryLayout::Multiput | EntryLayout::Sentinel => {
                let id = packet.entry().id;
                self.sent_writes.remove(&id)
            }
            EntryLayout::Read => {
                let read_loc = packet.entry().locs()[0];
                self.sent_reads.remove(&read_loc).map(WriteState::SingleServer)
            }
            EntryLayout::Lock => {
                // The only kind of send we do with a Lock layout is unlock
                // Without timeouts, failure indicates that someone else unlocked the server
                // for us, so we have nothing to do
                trace!("CLIENT unlock failure");
                None
            }
        };
        if let Some(state) = write_state {
            let re_add = self.server_for_token_mut(token).handle_redo(state, kind);
            if let Some(state) = re_add {
                assert!(state.is_multi());
                let id = state.id();
                self.sent_writes.insert(id, state);
            }
        }
    }

    fn add_unlocks(&mut self, mut buf: Rc<RefCell<Vec<u8>>>, lock_nums: Rc<HashMap<usize, u64>>) {
        match Rc::get_mut(&mut buf) {
            None => unreachable!(),
            Some(mut buf) => {
                let buf = buf.get_mut();
                trace!("buf pre clear {:?}", buf);
                buf.clear();
                trace!("buf after clear {:?}", buf);
                let lock = Lock {
                    //TODO id?
                    id: Uuid::nil(),
                    _padding: unsafe { mem::zeroed() },
                    kind: EntryKind::Lock,
                    lock: 0,
                };
                //TODO we shouldn't just free this...
                let lock = lock.bytes();
                trace!("lock bytes {:?}", lock);
                buf.extend_from_slice(lock);
                trace!("buflock {:?}", buf);
            }
        }
        assert_eq!(Entry::<()>::wrap_bytes(&buf.borrow()).kind.layout(), EntryLayout::Lock);
        assert_eq!(Entry::<()>::wrap_bytes(&buf.borrow()).entry_size(), buf.borrow().len());
        for (&server, &lock_num) in lock_nums.iter() {
            trace!("CLIENT add unlock for {:?}", (server, lock_num));
            let per_server = &mut self.servers[server];
            per_server.add_unlock(buf.clone(), lock_num);
            if !per_server.got_new_message {
                per_server.got_new_message = true;
                self.awake_io.push_back(server)
            }
        }
    }

    fn add_multis(&mut self, msg: Vec<u8>, lock_nums: HashMap<usize, u64>) {
        assert_eq!(Entry::<()>::wrap_bytes(&msg).entry_size(), msg.len());
        let lock_nums = Rc::new(lock_nums);
        let msg = Rc::new(RefCell::new(msg));
        for (&s, _) in lock_nums.iter() {
            let per_server = &mut self.servers[s];
            per_server.add_multi(msg.clone(), lock_nums.clone());
            if !per_server.got_new_message {
                per_server.got_new_message = true;
                self.awake_io.push_back(s)
            }
        }
    }

    fn is_single_node_append(&self, msg: &[u8]) -> bool {
        let mut single = true;
        let mut server_token = None;
        let locked_chains = Entry::<()>::wrap_bytes(&msg).locs()
            .iter().cloned().filter(|&oi| oi != OrderIndex(0.into(), 0.into()));
        for c in locked_chains {
            if let Some(server_token) = server_token {
                single &= self.server_for_chain(c.0) == server_token
            }
            else {
                server_token = Some(self.server_for_chain(c.0))
            }
        }
        single
    }

    fn get_lock_server_chains_for(&self, msg: &[u8]) -> Vec<OrderIndex> {
        //assert is multi?
        Entry::<()>::wrap_bytes(msg).locs()
            .iter().cloned()
            .filter(|&oi| oi != OrderIndex(0.into(), 0.into()))
            .fold((0..self.num_chain_servers)
                .map(|_| false).collect::<Vec<_>>(),
            |mut v, OrderIndex(o, _)| {
                v[self.server_for_chain(o)] = true;
                v
            }).into_iter()
            .enumerate()
            .filter_map(|(i, present)|
                if present { Some(OrderIndex((i as u32 + 1).into(), 0.into())) }
                else { None })
            .collect::<Vec<OrderIndex>>()
    }

    fn server_for_chain(&self, chain: order) -> usize {
        server_for_chain(chain, self.num_chain_servers)
    }

    fn server_for_token_mut(&mut self, token: Token) -> &mut PerServer<S> {
        &mut self.servers[token.0]
    }

    fn lock_token(&self) -> Token {
        Token(self.num_chain_servers)
    }

} // end impl AsyncStore

impl Connected for PerServer<TcpStream> {
    type Connection = TcpStream;
    fn connection(&self) -> &Self::Connection {
        &self.stream
    }

    fn recv_packet(&mut self) -> Result<Option<Buffer>, io::Error> {
        use std::io::ErrorKind;

        let bhs = base_header_size();
        //TODO I should make a nb_read macro...
        if self.bytes_read < bhs {
            let r = self.stream.read(&mut self.read_buffer[self.bytes_read..bhs]);
            match r {
                Ok(i) => self.bytes_read += i,
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock | io::ErrorKind::Interrupted => return Ok(None),
                    _ => return Err(e),
                }
            }
            if self.bytes_read < bhs {
                return Ok(None)
            }
        }

        let header_size = self.read_buffer.entry().header_size();
        assert!(header_size >= base_header_size());
        if self.bytes_read < header_size {
            let r = self.stream.read(&mut self.read_buffer[self.bytes_read..header_size]);
            match r {
                Ok(i) => self.bytes_read += i,
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock | io::ErrorKind::Interrupted => return Ok(None),
                    _ => return Err(e),
                }
            }
            if self.bytes_read < header_size {
                return Ok(None)
            }
        }

        let size = self.read_buffer.entry().entry_size();
        if self.bytes_read < size {
            let r = self.stream.read(&mut self.read_buffer[self.bytes_read..size]);
            match r {
                Ok(i) => self.bytes_read += i,
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock | io::ErrorKind::Interrupted => return Ok(None),
                    _ => return Err(e),
                }
            }
            if self.bytes_read < size {
                return Ok(None)
            }
        }
        debug_assert!(self.read_buffer.packet_fits());
        trace!("CLIENT finished recv");
        self.bytes_read = 0;
        let buff = mem::replace(&mut self.read_buffer, Buffer::new());
        Ok(Some(buff))
    }

    fn send_packet(&mut self, packet: &[u8]) -> bool {
        use std::io::ErrorKind;

        match self.stream.write(&packet[self.bytes_sent..]) {
            Ok(i) => self.bytes_sent += i,
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {}
                _ => panic!("CLIENT send error {}", e),
            }
        }
        let finished = self.bytes_sent >= packet.len();
        if finished { self.bytes_sent = 0 };
        finished
    }
}

impl Connected for PerServer<UdpConnection> {
    type Connection = UdpSocket;
    fn connection(&self) -> &Self::Connection {
        &self.stream.socket
    }

    fn recv_packet(&mut self) -> Result<Option<Buffer>, io::Error> {
        //TODO
        self.read_buffer.ensure_capacity(8192);
        //FIXME handle WouldBlock and number of bytes read
        let read = self.stream.socket.recv_from(&mut self.read_buffer[0..8192])
            .expect("cannot read");
        if let Some((read, _)) = read {
            if read > 0 {
                let buff = mem::replace(&mut self.read_buffer, Buffer::new());
                return Ok(Some(buff))
            }
        }

        Ok(None)
    }

    fn send_packet(&mut self, packet: &[u8]) -> bool {
        let addr = &self.stream.addr;
        //FIXME handle WouldBlock and number of bytes read
        let sent = self.stream.socket.send_to(packet, addr).expect("cannot write");
        if let Some(sent) = sent {
            return sent > 0
        }
        return false
    }
}

impl<S> PerServer<S>
where PerServer<S>: Connected {

    fn handle_redo(&mut self, failed: WriteState, _kind: EntryKind::Kind) -> Option<WriteState> {
        let to_ret = match &failed {
            f @ &WriteState::MultiServer(..) => Some(f.clone_multi()),
            _ => None,
        };
        //TODO front or back?
        self.awaiting_send.push_front(failed);
        to_ret
    }

    //FIXME add seperate write function which is split into TCP and UDP versions
    fn send_next_packet(&mut self, token: Token, num_servers: usize) -> Option<WriteState> {
        use self::WriteState::*;

        let send_in_progress = mem::replace(&mut self.currently_sending, None);
        if let Some(currently_sending) = send_in_progress {
            let finished = currently_sending.with_packet(|p| self.send_packet(p) );
            if finished {
                return Some(currently_sending)
            }
            else {
                mem::replace(&mut self.currently_sending, Some(currently_sending));
                return None
            }
        }

        match self.awaiting_send.pop_front() {
            None => None,
            Some(MultiServer(to_send, lock_nums)) => {
                let finished = {
                    trace!("CLIENT PerServer {:?} multi", token);
                    let mut ts = to_send.borrow_mut();
                    let kind = {
                        Entry::<()>::wrap_bytes(&*ts).kind
                    };
                    assert!(kind.layout() == EntryLayout::Multiput
                        || kind.layout() == EntryLayout::Sentinel);
                    assert!(kind.contains(EntryKind::TakeLock));
                    let lock_num = lock_nums.get(&token.0)
                        .expect("sending unlock for non-locked server");
                    let send_end = unsafe {
                        let e = Entry::<()>::wrap_bytes_mut(&mut *ts);
                        {
                            let is_data = e.locs().into_iter()
                                .take_while(|&&oi| oi != OrderIndex(0.into(), 0.into()))
                                .any(|oi| is_server_for(oi.0, token, num_servers));
                            let kind = &mut e.kind;
                            if is_data {
                                kind.remove(EntryKind::Lock);
                                assert_eq!(kind.layout(), EntryLayout::Multiput);
                            }
                            else {
                                kind.insert(EntryKind::Lock);
                                assert_eq!(kind.layout(), EntryLayout::Sentinel);
                            }
                            kind.insert(EntryKind::TakeLock);

                        }
                        e.as_multi_entry_mut().flex.lock = *lock_num;
                        //TODO this is either vec.len, or sentinelsize
                        e.entry_size()
                    };
                    let kind = {
                        Entry::<()>::wrap_bytes(&*ts).kind
                    };
                    assert!(kind.layout() == EntryLayout::Multiput
                        || kind.layout() == EntryLayout::Sentinel);
                    //TODO nonblocking writes
                    //Since sentinels have a different size than multis, we need to truncate
                    //for those sends
                    //self.stream.write_all(&ts[..send_end]).expect("cannot write");
                    self.send_packet(&ts[..send_end])
                };
                if finished {
                    Some(MultiServer(to_send, lock_nums))
                } else {
                    mem::replace(
                        &mut self.currently_sending,
                        Some(MultiServer(to_send, lock_nums))
                    );
                    return None
                }
            }
            Some(UnlockServer(to_send, lock_num)) => {
                let finished = {
                    trace!("CLIENT PerServer {:?} unlock", token);
                    let mut ts = to_send.borrow_mut();
                    let tslen = ts.len();
                    unsafe {
                        let e = Entry::<()>::wrap_bytes_mut(&mut *ts);
                        assert_eq!(e.kind.layout(), EntryLayout::Lock);
                        e.as_multi_entry_mut().flex.lock = lock_num;
                        assert_eq!(e.kind.layout(), EntryLayout::Lock);
                        assert_eq!(e.entry_size(), tslen);
                    }
                    trace!("CLIENT willsend {:?}", &*ts);
                    self.send_packet(&*ts)
                };
                if finished {
                    Some(UnlockServer(to_send, lock_num))
                } else {
                    mem::replace(
                        &mut self.currently_sending,
                        Some(UnlockServer(to_send, lock_num))
                    );
                    return None
                }
            }
            Some(to_send @ ToLockServer(_, _)) | Some(to_send @ SingleServer(_)) => {
                trace!("CLIENT PerServer {:?} single", token);
                {
                    let (l, _s) = to_send.with_packet(|p| {
                        let e = Entry::<()>::wrap_bytes(&*p);
                        (e.kind.layout(), e.entry_size())
                    });
                    assert!(l == EntryLayout::Data || l == EntryLayout::Multiput
                        || l == EntryLayout::Read)
                }
                //TODO multipart writes?
                let finished = if to_send.is_read() {
                    to_send.with_packet(|p| self.send_read_packet(p))
                }
                else {
                    to_send.with_packet(|p| self.send_packet(p))
                };
                
                trace!("CLIENT PerServer {:?} single written", token);
                if finished {
                    Some(to_send)
                } else {
                    mem::replace(&mut self.currently_sending, Some(to_send));
                    return None
                }
            }
        }
    }

    fn add_single_server_send(&mut self, msg: Vec<u8>) {
        self.awaiting_send.push_back(WriteState::SingleServer(msg));
    }

    fn add_multi(&mut self, msg: Rc<RefCell<Vec<u8>>>, lock_nums: Rc<HashMap<usize, u64>>) {
        self.awaiting_send.push_back(WriteState::MultiServer(msg, lock_nums));
    }

    fn add_unlock(&mut self, buffer: Rc<RefCell<Vec<u8>>>, lock_num: u64) {
        //unlike other reqs here we send the unlock first to minimize the contention window
        self.awaiting_send.push_front(WriteState::UnlockServer(buffer, lock_num))
    }

    fn add_get_lock_nums(&mut self, lock_req: Vec<u8>, msg: Vec<u8>) {
        self.awaiting_send.push_back(WriteState::ToLockServer(lock_req, msg))
    }

    fn needs_to_write(&self) -> bool {
        !self.awaiting_send.is_empty()
    }
}

impl WriteState {
    fn with_packet<F, R>(&self, f: F) -> R
    where F: for<'a> FnOnce(&'a [u8]) -> R {
        use self::WriteState::*;
        match self {
            &SingleServer(ref buf) | &ToLockServer(ref buf, _) => f(&**buf),
            &MultiServer(ref buf, _) | &UnlockServer(ref buf, _) => {
                let b = buf.borrow();
                f(&*b)
            },
        }
    }

    fn take(self) -> Vec<u8> {
        use self::WriteState::*;
        match self {
            SingleServer(buf) | ToLockServer(buf, _) => buf,
            MultiServer(buf, _) | UnlockServer(buf, _) =>
                Rc::try_unwrap(buf).expect("taking from non unique WriteState").into_inner()
        }
    }

    fn is_unlock(&self) -> bool {
        match self {
            &WriteState::UnlockServer(..) => true,
            _ => false,
        }
    }

    fn is_multi(&self) -> bool {
        match self {
            &WriteState::MultiServer(..) => true,
            _ => false,
        }
    }

    fn is_read(&self) -> bool {
        match self {
            &WriteState::SingleServer(ref vec) =>
                Entry::<()>::wrap_bytes(&vec[..]).kind.layout() == EntryLayout::Read,
            _ => false,
        }
    }

    fn id(&self) -> Uuid {
        let mut id = unsafe { mem::uninitialized() };
        self.with_packet(|p| {
            id = Entry::<()>::wrap_bytes(p).id
        });
        id
    }

    fn layout(&self) -> EntryLayout {
        let mut layout = unsafe { mem::uninitialized() };
        self.with_packet(|p| {
            layout = Entry::<()>::wrap_bytes(p).kind.layout();
        });
        layout
    }

    fn read_loc(&self) -> OrderIndex {
        let mut loc = unsafe { mem::uninitialized() };
        self.with_packet(|p| {
            let e = Entry::<()>::wrap_bytes(p);
            assert!(e.kind.layout() != EntryLayout::Lock);
            loc = e.locs()[0]
        });
        loc
    }

    fn clone_multi(&self) -> WriteState {
        use self::WriteState::*;
        match self {
            &MultiServer(ref b, ref l) => MultiServer(b.clone(), l.clone()),
            s => panic!("invlaid clone multi on {:?}", s)
        }

    }
}

fn server_for_chain(chain: order, num_servers: usize) -> usize {
    <u32 as From<order>>::from(chain) as usize % num_servers
}

fn is_server_for(chain: order, tok: Token, num_servers: usize) -> bool {
    server_for_chain(chain, num_servers) == tok.0
}
