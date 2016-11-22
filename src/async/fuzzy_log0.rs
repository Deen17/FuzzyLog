
//TODO use faster HashMap, HashSet
use std::{self, iter, mem};
use std::collections::{HashMap, VecDeque};
use std::collections::hash_map;
use std::rc::Rc;
use std::sync::mpsc;
use std::u32;

use bit_set::BitSet;

use mio;

use packets::*;
use async::tcp::AsyncStoreClient;
use self::FromStore::*;
use self::FromClient::*;

const MAX_PREFETCH: u32 = 8;

type ChainEntry = Rc<Vec<u8>>;

pub struct ThreadLog {
    to_store: mio::Sender<Vec<u8>>, //TODO send WriteState or other enum?
    from_outside: mpsc::Receiver<Message>, //TODO should this be per-chain?
    blockers: HashMap<OrderIndex, Vec<ChainEntry>>,
    blocked_multiappends: HashMap<Uuid, MultiSearchState>,
    per_chains: HashMap<order, PerChain>,
    //TODO replace with queue from deque to allow multiple consumers
    ready_reads: mpsc::Sender<Vec<u8>>,
    //TODO blocked_chains: BitSet ?
    //TODO how to multiplex writers finished_writes: Vec<mpsc::Sender<()>>,
    finished_writes: mpsc::Sender<(Uuid, Vec<OrderIndex>)>,
    //FIXME is currently unused
    to_return: VecDeque<Vec<u8>>,
    //TODO
    no_longer_blocked: Vec<OrderIndex>,
    cache: BufferCache,
    chains_currently_being_read: IsRead,
    num_snapshots: usize,
}

//TODO we could add messages from the client on read, and keep a counter of messages sent
//     this would allow us to ensure that every client gets an end-of-data message, as long
//     ad there're no concurrent snapshots...
struct PerChain {
    //TODO repr?
    //blocking: HashMap<entry, OrderIndex>,
    //read: VecDeque<ChainEntry>,
    //searching_for_multi_appends: HashMap<Uuid, OrderIndex>,
    //found_sentinels: HashSet<Uuid>,
    chain: order,
    last_snapshot: entry,
    last_read_sent_to_server: entry,
    outstanding_reads: u32, //TODO what size should this be
    //TODO is this necessary first_buffered: entry,
    last_returned_to_client: entry,
    blocked_on_new_snapshot: Option<Vec<u8>>,
    //FIXME this must be a counter
    num_multiappends_searching_for: usize,
    //TODO this is where is might be nice to have a more structured id format
    found_but_unused_multiappends: HashMap<Uuid, entry>,
    outstanding_snapshots: u32,
    is_being_read: Option<IsRead>,
}

type IsRead = Rc<()>;

struct MultiSearchState {
    val: Vec<u8>,
    pieces_remaining: usize,
}

pub enum Message {
    FromStore(FromStore),
    FromClient(FromClient),
}

//TODO hide in struct
pub enum FromStore {
    WriteComplete(Uuid, Vec<OrderIndex>), //TODO
    ReadComplete(OrderIndex, Vec<u8>),
}

pub enum FromClient {
    //TODO
    SnapshotAndPrefetch(order),
    PerformAppend(Vec<u8>),
    Shutdown,
}

enum MultiSearch {
    Finished(Vec<u8>),
    InProgress,
    EarlySentinel,
    BeyondHorizon(Vec<u8>),
    //MultiSearch::FirstPart(),
}

struct BufferCache {
    //TODO vec_cache: VecDeque<Vec<u8>>,
    //     rc_cache: VecDeque<Rc<Vec<u8>>>,
    //     alloced: usize,
    //     avg_alloced: usize,
}


impl ThreadLog {

    //TODO
    pub fn new<I>(to_store: mio::Sender<Vec<u8>>,
        from_outside: mpsc::Receiver<Message>,
        ready_reads: mpsc::Sender<Vec<u8>>,
        finished_writes: mpsc::Sender<(Uuid, Vec<OrderIndex>)>,
        interesting_chains: I)
    -> Self
    where I: IntoIterator<Item=order>{
        ThreadLog {
            to_store: to_store,
            from_outside: from_outside,
            blockers: Default::default(),
            blocked_multiappends: Default::default(),
            ready_reads: ready_reads,
            finished_writes: finished_writes,
            per_chains: interesting_chains.into_iter().map(|c| (c, PerChain::new(c))).collect(),
            to_return: Default::default(),
            no_longer_blocked: Default::default(),
            cache: BufferCache::new(),
            chains_currently_being_read: Default::default(),
            num_snapshots: 0,
        }
    }

    pub fn run(mut self) {
        loop {
            let msg = self.from_outside.recv().expect("outside is gone");
            if !self.handle_message(msg) { return }
        }
    }

    fn handle_message(&mut self, msg: Message) -> bool {
        match msg {
            Message::FromClient(msg) => self.handle_from_client(msg),
            Message::FromStore(msg) => self.handle_from_store(msg),
        }
    }

    fn handle_from_client(&mut self, msg: FromClient) -> bool {
        match msg {
            SnapshotAndPrefetch(chain) => {
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                self.fetch_snapshot(chain);
                self.prefetch(chain);
                true
            }
            PerformAppend(msg) => {
                {
                    let layout = bytes_as_entry(&msg).kind.layout();
                    assert!(layout == EntryLayout::Data || layout == EntryLayout::Multiput);
                }
                self.to_store.send(msg).expect("store hung up");
                true
            }
            Shutdown => {
                //TODO send shutdown
                false
            }
        }
    }

    fn handle_from_store(&mut self, msg: FromStore) -> bool {
        match msg {
            WriteComplete(id, locs) =>
                self.finished_writes.send((id, locs)).expect("client is gone"),
            ReadComplete(loc, msg) => self.handle_completed_read(loc, msg),
        }
        true
    }

    fn fetch_snapshot(&mut self, chain: order) {
        //XXX outstanding_snapshots is incremented in prefetch
        let packet = self.make_read_packet(chain, u32::MAX.into());
        self.to_store.send(packet).expect("store hung up")
    }

    fn prefetch(&mut self, chain: order) {
        //TODO allow new chains?
        //TODO how much to fetch
        let num_to_fetch = {
            let pc = &mut self.per_chains.get_mut(&chain).expect("boring server read");
            if pc.is_being_read.is_none() {
                pc.is_being_read = Some(self.chains_currently_being_read.clone());
            };
            pc.outstanding_snapshots += 1;
            let num_to_fetch = pc.num_to_fetch();
            let num_to_fetch = std::cmp::max(num_to_fetch, MAX_PREFETCH);
            let currently_buffering = pc.currently_buffering();
            //FIXME use outstanding reads
            if currently_buffering < num_to_fetch { num_to_fetch - currently_buffering }
            else { 0 }
        };
        for _ in 0..num_to_fetch {
            self.fetch_next(chain)
        }
    }

    fn handle_completed_read(&mut self, read_loc: OrderIndex, msg: Vec<u8>) {
        //TODO right now this assumes order...
        let kind = bytes_as_entry(&msg).kind;
        trace!("FUZZY handle read @ {:?}", read_loc);

        match kind.layout() {
            EntryLayout::Read => {
                trace!("FUZZY read has no data");
                debug_assert!(!kind.contains(EntryKind::ReadSuccess));
                debug_assert!(bytes_as_entry(&msg).locs()[0] == read_loc);
                if read_loc.1 < u32::MAX.into() {
                    trace!("FUZZY overread at {:?}", read_loc);
                    //TODO would be nice to handle ooo reads better...
                    //     we can probably do it by checking (chain, read_loc - 1)
                    //     to see if the read we're about to attempt is there, but
                    //     it might be better to switch to a buffer per-chain model
                    self.per_chains.get_mut(&read_loc.0).map(|s| {
                        s.overread_at(read_loc.1);
                        s.outstanding_reads -= 1;
                    });
                }
                else {
                    let unblocked = self.per_chains.get_mut(&read_loc.0).and_then(|s| {
                        let e = bytes_as_entry(&msg);
                        assert_eq!(e.locs()[0].1, u32::MAX.into());
                        debug_assert!(!e.kind.contains(EntryKind::ReadSuccess));
                        let new_horizon = e.dependencies()[0].1;
                        trace!("FUZZY try update horizon to {:?}", (read_loc.0, new_horizon));
                        s.outstanding_snapshots -= 1;
                        s.update_horizon(new_horizon)
                    });
                    if let Some(val) = unblocked {
                        let locs = self.return_entry(val);
                        if let Some(locs) = locs { self.stop_blocking_on(locs) }
                    }
                }
            }
            EntryLayout::Data => {
                trace!("FUZZY read is single");
                debug_assert!(kind.contains(EntryKind::ReadSuccess));
                //assert!(read_loc.1 >= pc.first_buffered);
                //TODO no-alloc?
                self.per_chains.get_mut(&read_loc.0).map(|s| s.outstanding_reads -= 1);
                let packet = Rc::new(msg);
                self.add_blockers(&packet);
                self.try_returning_at(read_loc, packet);
            }
            layout @ EntryLayout::Multiput | layout @ EntryLayout::Sentinel => {
                trace!("FUZZY read is multi");
                debug_assert!(kind.contains(EntryKind::ReadSuccess));
                self.per_chains.get_mut(&read_loc.0).map(|s| s.outstanding_reads -= 1);
                let is_sentinel = layout == EntryLayout::Sentinel;
                let search_status = self.update_multi_part_read(read_loc, msg, is_sentinel);
                match search_status {
                    MultiSearch::InProgress | MultiSearch::EarlySentinel => {}
                    MultiSearch::BeyondHorizon(..) => {
                        //TODO better ooo reads
                        self.per_chains.get_mut(&read_loc.0).expect("boring chain")
                            .overread_at(read_loc.1);
                    }
                    MultiSearch::Finished(msg) => {
                        //TODO no-alloc?
                        let packet = Rc::new(msg);
                        //TODO it would be nice to fetch the blockers in parallel...
                        //     we can add a fetch blockers call in update_multi_part_read
                        //     which updates the horizon but doesn't actually add the block
                        self.add_blockers(&packet);
                        self.try_returning(packet);
                    }
                }
            }

            EntryLayout::Lock => unreachable!(),
        }

        let finished_server = self.continue_fetch_if_needed(read_loc.0);
        if finished_server {
            trace!("FUZZY finished reading {:?}", read_loc.0);
            self.per_chains.get_mut(&read_loc.0).map(|pc| pc.is_being_read = None);
            if self.finshed_reading() {
                trace!("FUZZY finished reading all chains");
                //FIXME store the number of outstanding snapshots so we can return an end marker
                //      for each
                //FIXME add is_snapshoting to PerChain so this doesn't race?
                trace!("FUZZY finished reading");
                //TODO do we need a better system?
                let num_completeds = mem::replace(&mut self.num_snapshots, 0);
                assert!(num_completeds > 0);
                for _ in 0..num_completeds {
                    let _ = self.ready_reads.send(vec![]);
                }
            }
        }
    }

    /// Blocks a packet on entries a it depends on. Will increment the refcount for each
    /// blockage.
    fn add_blockers(&mut self, packet: &ChainEntry) {
        //FIXME dependencies currently assumes you gave it the correct type
        //      this is unnecessary and should be changed
        let entr = bytes_as_entry(packet);
        let deps = entr.dependencies();
        let locs = entr.locs();
        trace!("FUZZY checking {:?} for blockers in {:?}", locs, deps);
        for &(chain, index) in deps {
            let blocker_already_returned = self.per_chains.get_mut(&chain)
                .expect("read uninteresting chain")
                .has_returned(index);
            if !blocker_already_returned {
                trace!("FUZZY read @ {:?} blocked on {:?}", locs, (chain, index));
                //TODO no-alloc?
                let blocked = self.blockers.entry((chain, index)).or_insert_with(Vec::new);
                blocked.push(packet.clone());
            } else {
                trace!("FUZZY read @ {:?} need not wait for {:?}", locs, (chain, index));
            }
        }
        for &loc in locs {
            if loc.0 == order::from(0) { continue }
            let is_next_in_chain = self.per_chains.get(&loc.0)
                .expect("fetching uninteresting chain")
                .next_return_is(loc.1);
            if !is_next_in_chain {
                self.enqueue_packet(loc, packet.clone());
            }
        }
    }

    fn fetch_blockers_if_needed(&mut self, packet: &ChainEntry) {
        //TODO num_to_fetch
        //FIXME only do if below last_snapshot?
        let deps = bytes_as_entry(packet).dependencies();
        for &(chain, index) in deps {
            let unblocked;
            let num_to_fetch: u32 = {
                let pc = self.per_chains.get_mut(&chain)
                    .expect("tried reading uninteresting chain");
                unblocked = pc.update_horizon(index);
                pc.num_to_fetch()
            };
            trace!("FUZZY blocker {:?} needs {:?} additional reads", chain, num_to_fetch);
            for _ in 0..num_to_fetch {
                self.fetch_next(chain)
            }
            if let Some(val) = unblocked {
                let locs = self.return_entry(val);
                if let Some(locs) = locs { self.stop_blocking_on(locs) }
            }
        }
    }

    fn try_returning_at(&mut self, loc: OrderIndex, packet: ChainEntry) {
        match Rc::try_unwrap(packet) {
            Ok(e) => {
                trace!("FUZZY read {:?} is next", loc);
                if self.return_entry_at(loc, e) {
                    self.stop_blocking_on(iter::once(loc));
                }
            }
            //TODO should this be in add_blockers?
            Err(e) => self.fetch_blockers_if_needed(&e),
        }
    }

    fn try_returning(&mut self, packet: ChainEntry) {
        match Rc::try_unwrap(packet) {
            Ok(e) => {
                trace!("FUZZY returning next read?");
                if let Some(locs) = self.return_entry(e) {
                    trace!("FUZZY {:?} unblocked", locs);
                    self.stop_blocking_on(locs);
                }
            }
            //TODO should this be in add_blockers?
            Err(e) => self.fetch_blockers_if_needed(&e),
        }
    }

    fn stop_blocking_on<I>(&mut self, locs: I)
    where I: IntoIterator<Item=OrderIndex> {
        for loc in locs {
            if loc.0 == order::from(0) { continue }
            trace!("FUZZY unblocking reads after {:?}", loc);
            self.try_return_blocked_by(loc);
        }
        while let Some(loc) = self.no_longer_blocked.pop() {
            trace!("FUZZY continue unblocking reads after {:?}", loc);
            self.try_return_blocked_by(loc);
        }
    }

    fn try_return_blocked_by(&mut self, loc: OrderIndex) {
        //FIXME switch to using try_returning so needed fetches are done
        //      move up the stop_block loop into try_returning?
        let blocked = self.blockers.remove(&loc);
        if let Some(blocked) = blocked {
            for blocked in blocked.into_iter() {
                match Rc::try_unwrap(blocked) {
                    Ok(val) => {
                        {
                            let locs = bytes_as_entry(&val).locs();
                            trace!("FUZZY {:?} unblocked by {:?}", locs, loc);
                            self.no_longer_blocked.extend_from_slice(locs);
                        }
                        self.return_entry(val);
                    }
                    Err(still_blocked) =>
                        trace!("FUZZY {:?} no longer by {:?} but still blocked",
                            bytes_as_entry(&still_blocked).locs(), loc),
                }
            }
        }
    }

    fn update_multi_part_read(&mut self,
        read_loc: OrderIndex,
        mut msg: Vec<u8>,
        is_sentinel: bool)
    -> MultiSearch {
        let (id, num_pieces) = {
            let entr = bytes_as_entry(&msg);
            let id = entr.id;
            let locs = entr.locs();
            let num_pieces = locs.into_iter()
                .filter(|&&(o, _)| o != order::from(0))
                .count();
            trace!("FUZZY multi part read {:?} @ {:?}, {:?} pieces", id, locs, num_pieces);
            (id, num_pieces)
        };

        //TODO this should never really occur...
        if num_pieces == 1 {
            return MultiSearch::Finished(msg)
        }

        let is_later_piece = self.blocked_multiappends.contains_key(&id);
        if !is_later_piece && !is_sentinel {
            //FIXME I'm not sure if this is right
            if !self.per_chains[&read_loc.0].is_within_snapshot(read_loc.1) {
                trace!("FUZZY read multi too early @ {:?}", read_loc);
                return MultiSearch::BeyondHorizon(msg)
            }

            let mut pieces_remaining = num_pieces;
            trace!("FUZZY first part of multi part read");
            for &mut (o, ref mut i) in bytes_as_entry_mut(&mut msg).locs_mut() {
                if o != order::from(0) {
                    trace!("FUZZY fetching multi part @ {:?}?", (o, *i));
                    let early_sentinel = self.fetch_multi_parts(&id, o, *i);
                    if let Some(loc) = early_sentinel {
                        trace!("FUZZY no fetch @ {:?} sentinel already found", (o, *i));
                        assert!(loc != entry::from(0));
                        *i = loc;
                        pieces_remaining -= 1
                    }
                } else {
                    trace!("FUZZY no need to fetch multi part @ {:?}", (o, *i));
                }
            }

            if num_pieces == 0 {
                trace!("FUZZY all sentinels had already been found for {:?}", read_loc);
                return MultiSearch::Finished(msg)
            }

            trace!("FUZZY {:?} waiting for {:?} pieces", read_loc, num_pieces);
            self.blocked_multiappends.insert(id, MultiSearchState {
                val: msg,
                pieces_remaining: pieces_remaining
            });
        }
        else if !is_later_piece && is_sentinel {
            trace!("FUZZY early sentinel");
            self.per_chains.get_mut(&read_loc.0)
                .expect("boring chain")
                .add_early_sentinel(id, read_loc.1);
            return MultiSearch::EarlySentinel
        }
        else { trace!("FUZZY later part of multi part read"); }

        debug_assert!(self.per_chains[&read_loc.0].is_within_snapshot(read_loc.1));

        let was_blind_search;
        let finished = {
            if let hash_map::Entry::Occupied(mut found) = self.blocked_multiappends.entry(id) {
                let finished = {
                    let multi = found.get_mut();
                    let loc_ptr = bytes_as_entry_mut(&mut multi.val)
                        .locs_mut().into_iter()
                        .find(|&&mut (o, _)| o == read_loc.0)
                        .unwrap();
                    was_blind_search = loc_ptr.1 == entry::from(0);
                    *loc_ptr = read_loc;
                    multi.pieces_remaining -= 1;
                    trace!("FUZZY multi pieces remaining {:?}", multi.pieces_remaining);
                    multi.pieces_remaining == 0
                };
                match finished {
                    true => Some(found.remove().val),
                    false => None,
                }
            }
            else { unreachable!() }
        };

        //self.found_multi_part(read_loc.0, read_loc.1, was_blind_search);
        if was_blind_search {
            trace!("FUZZY finished blind seach for {:?}", read_loc);
            let pc = self.per_chains.get_mut(&read_loc.0).expect("tried reading boring chain");
            pc.decrement_multi_search();
        }

        match finished {
            Some(val) => {
                trace!("FUZZY finished multi part read");
                MultiSearch::Finished(val)
            }
            None => {
                trace!("FUZZY multi part read still waiting");
                MultiSearch::InProgress
            }
        }
    }

    fn fetch_multi_parts(&mut self, id: &Uuid, chain: order, index: entry) -> Option<entry> {
        //TODO argh, no-alloc
        let (unblocked, early_sentinel) = {
            let pc = self.per_chains.get_mut(&chain).expect("tried reading boring chain");

            let early_sentinel = pc.take_early_sentinel(&id);
            let potential_new_horizon = match early_sentinel {
                Some(loc) => loc,
                None => index,
            };

            //perform a non blind search if possible
            if index != entry::from(0) {
                trace!("RRRRR non-blind search {:?} {:?}", chain, index);
                let unblocked = pc.update_horizon(potential_new_horizon);
                (unblocked, early_sentinel)
            } else if early_sentinel.is_some() {
                trace!("RRRRR already found {:?} {:?}", chain, early_sentinel);
                //FIXME How does this interact with cached reads?
                (None, early_sentinel)
            } else {
                pc.increment_multi_search();
                trace!("RRRRR blind search {:?}", chain);
                (None, None)
            }
        };
        self.continue_fetch_if_needed(chain);

        if let Some(unblocked) = unblocked {
            //TODO no-alloc
            let locs = self.return_entry(unblocked);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }
        early_sentinel
    }

    fn continue_fetch_if_needed(&mut self, chain: order) -> bool {
        //TODO num_to_fetch
        let (num_to_fetch, unblocked) = {
            let pc = self.per_chains.get_mut(&chain).expect("boring chain");
            let num_to_fetch = pc.num_to_fetch();
            if num_to_fetch == 0 && pc.is_searching_for_multi() && pc.outstanding_reads == 0 {
                trace!("FUZZY {:?} updating horizon due to multi search", chain);
                (1, pc.increment_horizon())
            }
            else {
                trace!("FUZZY {:?} needs {:?} additional reads", chain, num_to_fetch);
                (num_to_fetch, None)
            }
        };

        for _ in 0..num_to_fetch {
            //FIXME check if we have a cached version before issuing fetch
            //      laking this can cause unsound behzvior on multipart reads
            self.fetch_next(chain)
        }

        if let Some(unblocked) = unblocked {
            //TODO no-alloc
            let locs = self.return_entry(unblocked);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }

        self.server_is_finished(chain)
    }

    fn enqueue_packet(&mut self, loc: OrderIndex, packet: ChainEntry) {
        assert!(loc.1 > 1.into());
        debug_assert!(self.per_chains.get(&loc.0).unwrap().last_returned_to_client < loc.1 - 1);
        let blocked_on = (loc.0, loc.1 - 1);
        trace!("FUZZY read @ {:?} blocked on prior {:?}", loc, blocked_on);
        //TODO no-alloc?
        let blocked = self.blockers.entry(blocked_on).or_insert_with(Vec::new);
        blocked.push(packet.clone());
    }

    fn return_entry_at(&mut self, loc: OrderIndex, val: Vec<u8>) -> bool {
        debug_assert!(bytes_as_entry(&val).locs()[0] == loc);
        debug_assert!(bytes_as_entry(&val).locs().len() == 1);
        trace!("FUZZY trying to return read @ {:?}", loc);
        let (o, i) = loc;
        {
            let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
            if !pc.is_within_snapshot(i) {
                trace!("FUZZY blocking read @ {:?}, waiting for snapshot", loc);
                pc.block_on_snapshot(val);
                return false
            }

            trace!("QQQQQ setting returned {:?}", (o, i));
            pc.set_returned(i);
        };
        trace!("FUZZY returning read @ {:?}", loc);
        //FIXME first_buffered?
        self.ready_reads.send(val).expect("client hung up");
        true
    }

    ///returns None if return stalled Some(Locations which are now unblocked>) if return
    ///        succeeded
    //TODO it may make sense to change these funtions to add the returned messages to an
    //     internal ring which can be used to discover the unblocked entries before the
    //     messages are flushed to the client, as this would remove the intermidate allocation
    //     and it may be a bit nicer
    fn return_entry(&mut self, val: Vec<u8>) -> Option<Vec<OrderIndex>> {
        let locs = {
            let mut should_block_on = None;
            {
                let locs = bytes_as_entry(&val).locs();
                trace!("FUZZY trying to return read from {:?}", locs);
                for &(o, i) in locs.into_iter() {
                    if o == order::from(0) { continue }
                    let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                    if !pc.is_within_snapshot(i) {
                        trace!("FUZZY must block read @ {:?}, waiting for snapshot", (o, i));
                        should_block_on = Some(o);
                    }
                }
            }
            if let Some(o) = should_block_on {
                let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                pc.block_on_snapshot(val);
                return None
            }
            let locs = bytes_as_entry(&val).locs();
            for &(o, i) in locs.into_iter() {
                if o == order::from(0) { continue }
                trace!("QQQQ setting returned {:?}", (o, i));
                let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                debug_assert!(pc.is_within_snapshot(i));
                pc.set_returned(i);
            }
            //TODO no-alloc
            //     a better solution might be to have this function push onto a temporary
            //     VecDeque who's head is used to unblock further entries, and is then sent
            //     to the client
            locs.to_vec()
        };
        trace!("FUZZY returning read @ {:?}", locs);
        //FIXME first_buffered?
        self.ready_reads.send(val).expect("client hung up");
        Some(locs)
    }

    fn fetch_next(&mut self, chain: order) {
        let next = {
            let per_chain = &mut self.per_chains.get_mut(&chain)
                .expect("fetching uninteresting chain");
            //TODO this is no a great place for this?
            //TODO maybe a bitmask instead?
            if per_chain.is_being_read.is_none() {
                trace!("RRRRR {:?} is now being read", chain);
                per_chain.is_being_read = Some(self.chains_currently_being_read.clone());
            };
            per_chain.last_read_sent_to_server = per_chain.last_read_sent_to_server + 1;
            per_chain.outstanding_reads += 1;
            per_chain.last_read_sent_to_server
        };
        let packet = self.make_read_packet(chain, next);
        self.to_store.send(packet).expect("store hung up")
    }

    fn make_read_packet(&mut self, chain: order, index: entry) -> Vec<u8> {
        let mut buffer = self.cache.alloc();
        {
            let e = EntryContents::Data(&(), &[]).fill_vec(&mut buffer);
            e.kind = EntryKind::Read;
            e.locs_mut()[0] = (chain, index);
        }
        buffer
    }

    fn finshed_reading(&mut self) -> bool {
        let finished = Rc::get_mut(&mut self.chains_currently_being_read).is_some();
        //FIXME this is dumb, it might be better to have a counter of how many servers we are
        //      waiting for
        debug_assert_eq!({
            let mut currently_being_read = 0;
            for (_, pc) in self.per_chains.iter() {
                if !pc.is_finished() {
                    currently_being_read += 1
                }
                //still_reading |= pc.has_outstanding_reads()
            }
            // !still_reading == (self.servers_currently_being_read == 0)
            currently_being_read == 0
        }, finished);

        finished
    }

    fn server_is_finished(&self, chain: order) -> bool {
        let pc = &self.per_chains[&chain];
        assert!(!(pc.outstanding_reads == 0
            && pc.last_read_sent_to_server < pc.last_snapshot));
        assert!(!(pc.is_searching_for_multi() && !pc.has_outstanding_reads()));
        pc.is_finished()
    }
}

impl PerChain {
    fn new(chain: order) -> Self {
        PerChain {
            chain: chain,
            last_snapshot: 0.into(),
            last_read_sent_to_server: 0.into(),
            outstanding_reads: 0,
            last_returned_to_client: 0.into(),
            blocked_on_new_snapshot: None,
            num_multiappends_searching_for: 0,
            found_but_unused_multiappends: Default::default(),
            outstanding_snapshots: 0,
            is_being_read: None,
        }
    }

    fn set_returned(&mut self, index: entry) {
        assert!(self.next_return_is(index));
        assert!(index > self.last_returned_to_client);
        assert!(index <= self.last_snapshot);
        trace!("QQQQQ returning {:?}", (self.chain, index));
        self.last_returned_to_client = index;
    }

    fn overread_at(&mut self, index: entry) {
        // The conditional is needed because sends we sent before reseting
        // last_read_sent_to_server race future calls to this function
        if self.last_read_sent_to_server > index
            && self.last_read_sent_to_server > self.last_returned_to_client {
            trace!("FUZZY resetting read loc for {:?} from {:?} to {:?}",
                self.chain, self.last_read_sent_to_server, index);
            self.last_read_sent_to_server = index - 1
        }
    }

    fn can_return(&self, index: entry) -> bool {
        self.next_return_is(index) && self.is_within_snapshot(index)
    }

    fn has_returned(&mut self, index: entry) -> bool {
        trace!{"QQQQQ last return for {:?}: {:?}", self.chain, self.last_returned_to_client};
        index <= self.last_returned_to_client
    }

    fn next_return_is(&self, index: entry) -> bool {
        trace!("QQQQQ next return for {:?}: {:?}", self.chain, self.last_returned_to_client + 1);
        index == self.last_returned_to_client + 1
    }

    fn is_within_snapshot(&self, index: entry) -> bool {
        trace!("QQQQQ {:?}: {:?} <= {:?}", self.chain, index, self.last_snapshot);
        index <= self.last_snapshot
    }

    fn is_searching_for_multi(&self) -> bool {
        self.num_multiappends_searching_for > 0
    }

    fn increment_horizon(&mut self) -> Option<Vec<u8>> {
        let new_horizon = self.last_snapshot + 1;
        self.update_horizon(new_horizon)
    }

    fn update_horizon(&mut self, new_horizon: entry) -> Option<Vec<u8>> {
        if self.last_snapshot < new_horizon {
            trace!("FUZZY update horizon {:?}", (self.chain, new_horizon));
            self.last_snapshot = new_horizon;
            if entry_is_unblocked(&self.blocked_on_new_snapshot, self.chain, new_horizon) {
                trace!("FUZZY unblocked entry");
                return mem::replace(&mut self.blocked_on_new_snapshot, None)
            }
        }
        else {
            trace!("FUZZY needless horizon update for {:?}: {:?} <= {:?}",
                self.chain, new_horizon, self.last_snapshot);
        }

        return None;

        fn entry_is_unblocked(val: &Option<Vec<u8>>, chain: order, new_horizon: entry) -> bool {
            val.as_ref().map_or(false, |v| {
                let locs = bytes_as_entry(v).locs();
                for &(o, i) in locs {
                    if o == chain && i <= new_horizon {
                        return true
                    }
                }
                false
            })
        }
    }

    fn block_on_snapshot(&mut self, val: Vec<u8>) {
        debug_assert!(bytes_as_entry(&val).locs().into_iter()
            .find(|&&(o, _)| o == self.chain).unwrap().1 == self.last_snapshot + 1);
        assert!(self.blocked_on_new_snapshot.is_none());
        self.blocked_on_new_snapshot = Some(val)
    }

    fn num_to_fetch(&self) -> u32 {
        //TODO switch to saturating sub?
        assert!(self.last_returned_to_client <= self.last_snapshot,
            "FUZZY returned value early. {:?} should be less than {:?}",
            self.last_returned_to_client, self.last_snapshot);
        if self.last_read_sent_to_server <= self.last_snapshot {
            (self.last_snapshot - self.last_read_sent_to_server.into()).into()

        } else {
            0
        }
        /*
        //FIXME this should be based on the number of requests outstanding from the server
        //     only if the number of requests is zero do we read beyond the horizon
        if self.num_multiappends_searching_for > 0 { 1 } else { 0 }
        */

    }

    fn currently_buffering(&self) -> u32 {
        //TODO switch to saturating sub?
        let currently_buffering = self.last_read_sent_to_server
            - self.last_returned_to_client.into();
        let currently_buffering: u32 = currently_buffering.into();
        currently_buffering
    }

    fn increment_multi_search(&mut self) {
        self.num_multiappends_searching_for += 1;
        trace!("QQQQQ {:?} + now searching for {:?} multis",
            self.chain, self.num_multiappends_searching_for);
    }

    fn decrement_multi_search(&mut self) {
        assert!(self.num_multiappends_searching_for > 0);
        self.num_multiappends_searching_for -= 1;
        trace!("QQQQQ {:?} - now searching for {:?} multis",
            self.chain, self.num_multiappends_searching_for);
    }

    fn add_early_sentinel(&mut self, id: Uuid, index: entry) {
        assert!(index != 0.into());
        let old = self.found_but_unused_multiappends.insert(id, index);
        debug_assert!(old.is_none());
    }

    fn take_early_sentinel(&mut self, id: &Uuid) -> Option<entry> {
        self.found_but_unused_multiappends.remove(id)
    }

    fn has_outstanding_reads(&self) -> bool {
        self.outstanding_reads > 0
    }

    fn has_outstanding_snapshots(&self) -> bool {
        self.outstanding_snapshots > 0
    }

    fn is_finished(&self) -> bool {
        assert!(!(self.outstanding_reads == 0
            && self.last_read_sent_to_server != self.last_snapshot));
        !(self.has_outstanding_reads() || self.is_searching_for_multi()
        || self.has_outstanding_snapshots())
    }
}

impl BufferCache {
    fn new() -> Self {
        BufferCache{}
    }

    fn alloc(&mut self) -> Vec<u8> {
        //TODO
        Vec::new()
    }
}

impl AsyncStoreClient for mpsc::Sender<Message> {
    fn on_finished_read(&mut self, read_loc: OrderIndex, read_packet: Vec<u8>) {
        let _ = self.send(Message::FromStore(ReadComplete(read_loc, read_packet)));
    }

    //TODO what info is needed?
    fn on_finished_write(&mut self, write_id: Uuid, write_locs: Vec<OrderIndex>) {
        let _ = self.send(Message::FromStore(WriteComplete(write_id, write_locs)));
    }
}

#[cfg(test)]
mod tests {
    use packets::*;
    use super::*;
    use super::FromClient::*;
    use async::tcp::AsyncTcpStore;

    use std::collections::HashMap;
    use std::marker::PhantomData;
    use std::{mem, thread};
    use std::sync::{mpsc, Arc, Mutex};

    use mio::{self, EventLoop};

    //TODO move to crate root under cfg...
    extern crate env_logger;

    #[test]
    fn test_get_none() {
        let _ = env_logger::init();
        let mut lh = new_thread_log::<()>(vec![1.into()]);
        lh.snapshot(1.into());
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_1_column() {
        let _ = env_logger::init();
        trace!("TEST 1 column");
        let mut lh = new_thread_log::<i32>(vec![3.into()]);
        let _ = lh.append(3.into(), &1, &[]);
        let _ = lh.append(3.into(), &17, &[]);
        let _ = lh.append(3.into(), &32, &[]);
        let _ = lh.append(3.into(), &-1, &[]);
        lh.snapshot(3.into());
        assert_eq!(lh.get_next(), Some((&1,  &[(3.into(), 1.into())][..])));
        assert_eq!(lh.get_next(), Some((&17, &[(3.into(), 2.into())][..])));
        assert_eq!(lh.get_next(), Some((&32, &[(3.into(), 3.into())][..])));
        assert_eq!(lh.get_next(), Some((&-1, &[(3.into(), 4.into())][..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_3_column() {
        let _ = env_logger::init();
        trace!("TEST 3 column");

        let mut lh = new_thread_log::<i32>(vec![4.into(), 5.into(), 6.into()]);
        let cols = vec![vec![12, 19, 30006, 122, 9],
            vec![45, 111111, -64, 102, -10101],
            vec![-1, -2, -9, 16, -108]];
        for (j, col) in cols.iter().enumerate() {
            for i in col.iter() {
                let _ = lh.append(((j + 4) as u32).into(), i, &[]);
            }
        }
        lh.snapshot(4.into());
        lh.snapshot(6.into());
        lh.snapshot(5.into());
        let mut is = [0u32, 0, 0, 0];
        let total_len = cols.iter().fold(0, |len, col| len + col.len());
        for _ in 0..total_len {
            let next = lh.get_next();
            assert!(next.is_some());
            let (&n, ois) = next.unwrap();
            assert_eq!(ois.len(), 1);
            let (o, i) = ois[0];
            let off: u32 = (o - 4).into();
            is[off as usize] = is[off as usize] + 1;
            let i: u32 = i.into();
            assert_eq!(is[off as usize], i);
            let c = is[off as usize] - 1;
            assert_eq!(n, cols[off as usize][c as usize]);
        }
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_read_deps() {
        let _ = env_logger::init();
        trace!("TEST read deps");

        let mut lh = new_thread_log::<i32>(vec![7.into(), 8.into()]);

        let _ = lh.append(7.into(), &63,  &[]);
        let _ = lh.append(8.into(), &-2,  &[(7.into(), 1.into())]);
        let _ = lh.append(8.into(), &-56, &[]);
        let _ = lh.append(7.into(), &111, &[(8.into(), 2.into())]);
        let _ = lh.append(8.into(), &0,   &[(7.into(), 2.into())]);
        lh.snapshot(8.into());
        lh.snapshot(7.into());
        assert_eq!(lh.get_next(), Some((&63,  &[(7.into(), 1.into())][..])));
        assert_eq!(lh.get_next(), Some((&-2,  &[(8.into(), 1.into())][..])));
        assert_eq!(lh.get_next(), Some((&-56, &[(8.into(), 2.into())][..])));
        assert_eq!(lh.get_next(), Some((&111, &[(7.into(), 2.into())][..])));
        assert_eq!(lh.get_next(), Some((&0,   &[(8.into(), 3.into())][..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_long() {
        let _ = env_logger::init();
        trace!("TEST long");

        let mut lh = new_thread_log::<i32>(vec![9.into()]);
        for i in 0..19i32 {
            let _ = lh.append(9.into(), &i, &[]);
        }
        lh.snapshot(9.into());
        for i in 0..19i32 {
            let u = i as u32;
            assert_eq!(lh.get_next(), Some((&i,  &[(9.into(), (u + 1).into())][..])));
        }
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_wide() {
        let _ = env_logger::init();
        trace!("TEST wide");

        let interesting_chains: Vec<_> = (10..21).map(|i| i.into()).collect();
        let mut lh = new_thread_log(interesting_chains.clone());
        for &i in &interesting_chains {
            if i > 10.into() {
                let _ = lh.append(i.into(), &i, &[(i - 1, 1.into())]);
            }
            else {
                let _ = lh.append(i.into(), &i, &[]);
            }

        }
        lh.snapshot(20.into());
        for &i in &interesting_chains {
            assert_eq!(lh.get_next(), Some((&i,  &[(i, 1.into())][..])));
        }
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_append_after_fetch() {
        let _ = env_logger::init();
        trace!("TEST append after fetch");

        let mut lh = new_thread_log(vec![21.into()]);
        for i in 0u32..10 {
            let _ = lh.append(21.into(), &i, &[]);
        }
        lh.snapshot(21.into());
        for i in 0u32..10 {
            assert_eq!(lh.get_next(), Some((&i,  &[(21.into(), (i + 1).into())][..])));
        }
        assert_eq!(lh.get_next(), None);
        for i in 10u32..21 {
            let _ = lh.append(21.into(), &i, &[]);
        }
        lh.snapshot(21.into());
        for i in 10u32..21 {
            assert_eq!(lh.get_next(), Some((&i,  &[(21.into(), (i + 1).into())][..])));
        }
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_append_after_fetch_short() {
        let _ = env_logger::init();
        trace!("TEST append after fetch short");

        let mut lh = new_thread_log(vec![22.into()]);
        for i in 0u32..2 {
            let _ = lh.append(22.into(), &i, &[]);
        }
        lh.snapshot(22.into());
        for i in 0u32..2 {
            assert_eq!(lh.get_next(), Some((&i,  &[(22.into(), (i + 1).into())][..])));
        }
        assert_eq!(lh.get_next(), None);
        for i in 2u32..4 {
            let _ = lh.append(22.into(), &i, &[]);
        }
        lh.snapshot(22.into());
        for i in 2u32..4 {
            assert_eq!(lh.get_next(), Some((&i,  &[(22.into(), (i + 1).into())][..])));
        }
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_multi() {
        let _ = env_logger::init();
        trace!("TEST multi");

        let columns = vec![23.into(), 24.into(), 25.into()];
        let mut lh = new_thread_log::<u64>(columns.clone());
        let _ = lh.multiappend(&columns, &0xfeed, &[]);
        let _ = lh.multiappend(&columns, &0xbad , &[]);
        let _ = lh.multiappend(&columns, &0xcad , &[]);
        let _ = lh.multiappend(&columns, &13    , &[]);
        lh.snapshot(24.into());
        assert_eq!(lh.get_next(), Some((&0xfeed, &[(23.into(), 1.into()),
            (24.into(), 1.into()), (25.into(), 1.into())][..])));
        assert_eq!(lh.get_next(), Some((&0xbad , &[(23.into(), 2.into()),
            (24.into(), 2.into()), (25.into(), 2.into())][..])));
        assert_eq!(lh.get_next(), Some((&0xcad , &[(23.into(), 3.into()),
            (24.into(), 3.into()), (25.into(), 3.into())][..])));
        assert_eq!(lh.get_next(), Some((&13    , &[(23.into(), 4.into()),
            (24.into(), 4.into()), (25.into(), 4.into())][..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_multi_shingled() {
        let _ = env_logger::init();
        trace!("TEST multi shingled");

        let columns = vec![26.into(), 27.into(), 28.into(), 29.into(), 30.into()];
        let mut lh = new_thread_log::<u64>(columns.clone());
        for (i, cols) in columns.windows(2).rev().enumerate() {
            let i = i as u64;
            let _ = lh.multiappend(&cols, &((i + 1) * 2), &[]);
        }
        lh.snapshot(26.into());
        assert_eq!(lh.get_next(),
            Some((&2, &[(29.into(), 1.into()), (30.into(), 1.into())][..])));
        assert_eq!(lh.get_next(),
            Some((&4, &[(28.into(), 1.into()), (29.into(), 2.into())][..])));
        assert_eq!(lh.get_next(),
            Some((&6, &[(27.into(), 1.into()), (28.into(), 2.into())][..])));
        assert_eq!(lh.get_next(),
            Some((&8, &[(26.into(), 1.into()), (27.into(), 2.into())][..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_multi_wide() {
        let _ = env_logger::init();
        trace!("TEST multi wide");

        let columns: Vec<_> = (31..45).map(Into::into).collect();
        let mut lh = new_thread_log::<u64>(columns.clone());
        let _ = lh.multiappend(&columns, &82352  , &[]);
        let _ = lh.multiappend(&columns, &018945 , &[]);
        let _ = lh.multiappend(&columns, &119332 , &[]);
        let _ = lh.multiappend(&columns, &0      , &[]);
        let _ = lh.multiappend(&columns, &17     , &[]);
        lh.snapshot(33.into());
        let locs: Vec<_> = columns.iter().map(|&o| (o, 1.into())).collect();
        assert_eq!(lh.get_next(), Some((&82352 , &locs[..])));
        let locs: Vec<_> = columns.iter().map(|&o| (o, 2.into())).collect();
        assert_eq!(lh.get_next(), Some((&018945, &locs[..])));
        let locs: Vec<_> = columns.iter().map(|&o| (o, 3.into())).collect();
        assert_eq!(lh.get_next(), Some((&119332, &locs[..])));
        let locs: Vec<_> = columns.iter().map(|&o| (o, 4.into())).collect();
        assert_eq!(lh.get_next(), Some((&0     , &locs[..])));
        let locs: Vec<_> = columns.iter().map(|&o| (o, 5.into())).collect();
        assert_eq!(lh.get_next(), Some((&17    , &locs[..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_multi_deep() {
        let _ = env_logger::init();
        trace!("TEST multi deep");

        let columns: Vec<_> = (45..49).map(Into::into).collect();
        let mut lh = new_thread_log::<u32>(columns.clone());
        for i in 1..32 {
            let _ = lh.multiappend(&columns, &i, &[]);
        }
        lh.snapshot(48.into());
        for i in 1..32 {
            let locs: Vec<_> = columns.iter().map(|&o| (o, i.into())).collect();
            assert_eq!(lh.get_next(), Some((&i , &locs[..])));
        }
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_dependent_multi() {
        let _ = env_logger::init();
        trace!("TEST multi");

        let columns = vec![49.into(), 50.into(), 51.into()];
        let mut lh = new_thread_log::<u64>(columns.clone());
        let _ = lh.append(50.into(), &22, &[]);
        let _ = lh.append(51.into(), &11, &[]);
        let _ = lh.append(49.into(), &0xf0000, &[]);
        let _ = lh.dependent_multiappend(&[49.into()], &[50.into(), 51.into()], &0xbaaa, &[]);
        lh.snapshot(49.into());
        {
            let potential_vals: [_; 3] =
                [(22     , vec![(50.into(), 1.into())]),
                 (11     , vec![(51.into(), 1.into())]),
                 (0xf0000, vec![(49.into(), 1.into())])
                ];
            let mut potential_vals: HashMap<_, _> = potential_vals.into_iter().cloned().collect();
            for _ in 0..3 {
                let next_val = &lh.get_next().expect("should find val");
                let locs = potential_vals.remove(next_val.0).expect("must be expected");
                assert_eq!(next_val.1, &locs[..]);
            }
        }
        assert_eq!(lh.get_next(),
            Some((&0xbaaa,
                &[(49.into(), 2.into()),
                  ( 0.into(), 0.into()),
                  (50.into(), 2.into()),
                  (51.into(), 2.into())
                 ][..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_dependent_multi_with_early_fetch() {
        let _ = env_logger::init();
        trace!("TEST multi");

        let columns = vec![52.into(), 53.into(), 54.into()];
        let mut lh = new_thread_log::<i64>(columns.clone());
        let _ = lh.append(52.into(), &99999, &[]);
        let _ = lh.append(53.into(), &101, &[]);
        let _ = lh.append(54.into(), &-99, &[]);
        let _ = lh.dependent_multiappend(&[53.into()], &[52.into(), 54.into()], &-7777, &[]);
        lh.snapshot(52.into());
        lh.snapshot(54.into());
        {
            let potential_vals =
                [(99999, vec![(52.into(), 1.into())]),
                 (-99  , vec![(54.into(), 1.into())]),
                ];
            let mut potential_vals: HashMap<_, _> = potential_vals.into_iter().cloned().collect();
            for _ in 0..2 {
                let next_val = &lh.get_next().expect("should find val");
                match potential_vals.remove(next_val.0) {
                    Some(locs) => assert_eq!(next_val.1, &locs[..]),
                    None => panic!("unexpected val {:?}", next_val),
                }

            }
        }
        lh.snapshot(53.into());
        assert_eq!(lh.get_next(), Some((&101, &[(53.into(), 1.into())][..])));
        assert_eq!(lh.get_next(),
            Some((&-7777,
                &[(53.into(), 2.into()),
                  ( 0.into(), 0.into()),
                  (52.into(), 2.into()),
                  (54.into(), 2.into())
                 ][..])));
        assert_eq!(lh.get_next(), None);
    }

    #[test]
    pub fn test_dependent_multi_with_partial_early_fetch() {
        let _ = env_logger::init();
        trace!("TEST multi");

        let columns = vec![55.into(), 56.into(), 57.into()];
        let mut lh = new_thread_log::<i64>(columns.clone());
        let _ = lh.append(55.into(), &99999, &[]);
        let _ = lh.append(56.into(), &101, &[]);
        let _ = lh.append(57.into(), &-99, &[]);
        let _ = lh.dependent_multiappend(&[55.into()], &[56.into(), 57.into()], &-7777, &[]);
        lh.snapshot(56.into());
        assert_eq!(lh.get_next(), Some((&101, &[(56.into(), 1.into())][..])));
        lh.snapshot(55.into());
        assert_eq!(lh.get_next(), Some((&99999, &[(55.into(), 1.into())][..])));
        assert_eq!(lh.get_next(), Some((&-99, &[(57.into(), 1.into())][..])));
        assert_eq!(lh.get_next(),
            Some((&-7777,
                &[(55.into(), 2.into()),
                  ( 0.into(), 0.into()),
                  (56.into(), 2.into()),
                  (57.into(), 2.into())
                 ][..])));
        assert_eq!(lh.get_next(), None);
    }

    //TODO test append after prefetch but before read

    /*

    #[test]
    fn test_1_column_ni() {
        let _ = env_logger::init();
        let store = new_store(
            vec![(4.into(), 1.into()), (4.into(), 2.into()),
                (4.into(), 3.into()), (5.into(), 1.into())]
        );
        let horizon = HashMap::new();
        let map = Rc::new(RefCell::new(HashMap::new()));
        let mut upcalls: HashMap<_, Box<for<'u, 'o, 'r> Fn(&'u Uuid, &'o OrderIndex, &'r _) -> bool>> = HashMap::new();
        let re = map.clone();
        upcalls.insert(4.into(), Box::new(move |_, _, &MapEntry(k, v)| {
            re.borrow_mut().insert(k, v);
            true
        }));
        upcalls.insert(5.into(), Box::new(|_, _, _| false));

        let mut log = FuzzyLog::new(store, horizon, upcalls);
        let e1 = log.append(4.into(), &MapEntry(0, 1), &*vec![]);
        assert_eq!(e1, (4.into(), 1.into()));
        let e2 = log.append(4.into(), &MapEntry(1, 17), &*vec![]);
        assert_eq!(e2, (4.into(), 2.into()));
        let last_index = log.append(4.into(), &MapEntry(32, 5), &*vec![]);
        assert_eq!(last_index, (4.into(), 3.into()));
        let en = log.append(5.into(), &MapEntry(0, 0), &*vec![last_index]);
        assert_eq!(en, (5.into(), 1.into()));
        log.play_foward(4.into());
        assert_eq!(*map.borrow(), [(0,1), (1,17), (32,5)].into_iter().cloned().collect());
    }

    #[test]
    fn test_deps() {
        let _ = env_logger::init();
        let store = new_store(
            vec![(6.into(), 1.into()), (6.into(), 2.into()),
                (6.into(), 3.into()), (7.into(), 1.into())]
        );
        let horizon = HashMap::new();
        let map = Rc::new(RefCell::new(HashMap::new()));
        let mut upcalls: HashMap<_, Box<for<'u, 'o, 'r> Fn(&'u Uuid, &'o OrderIndex, &'r _) -> bool>> = HashMap::new();
        let re = map.clone();
        upcalls.insert(6.into(), Box::new(move |_, _, &MapEntry(k, v)| {
            re.borrow_mut().insert(k, v);
            true
        }));
        upcalls.insert(7.into(), Box::new(|_, _, _| false));
        let mut log = FuzzyLog::new(store, horizon, upcalls);
        let e1 = log.append(6.into(), &MapEntry(0, 1), &*vec![]);
        assert_eq!(e1, (6.into(), 1.into()));
        let e2 = log.append(6.into(), &MapEntry(1, 17), &*vec![]);
        assert_eq!(e2, (6.into(), 2.into()));
        let last_index = log.append(6.into(), &MapEntry(32, 5), &*vec![]);
        assert_eq!(last_index, (6.into(), 3.into()));
        let en = log.append(7.into(), &MapEntry(0, 0), &*vec![last_index]);
        assert_eq!(en, (7.into(), 1.into()));
        log.play_foward(7.into());
        assert_eq!(*map.borrow(), [(0,1), (1,17), (32,5)].into_iter().cloned().collect());
    }

    #[test]
    fn test_order() {
        let _ = env_logger::init();
        let store = new_store(
            (0..5).map(|i| (20.into(), i.into()))
                .chain((0..21).map(|i| (21.into(), i.into())))
                .chain((0..22).map(|i| (22.into(), i.into())))
                .collect());
        let horizon = HashMap::new();
        let list: Rc<RefCell<Vec<i32>>> = Default::default();
        let mut upcalls: HashMap<_, Box<for<'u, 'o, 'r> Fn(&'u Uuid, &'o OrderIndex, &'r _) -> bool>> = Default::default();
        for i in 20..23 {
            let l = list.clone();
            upcalls.insert(i.into(), Box::new(move |_,_,&v| { l.borrow_mut().push(v);
                true
            }));
        }
        let mut log = FuzzyLog::new(store, horizon, upcalls);
        log.append(22.into(), &4, &[]);
        log.append(20.into(), &2, &[]);
        log.append(21.into(), &3, &[]);
        log.multiappend(&[20.into(),21.into(),22.into()], &-1, &[]);
        log.play_foward(20.into());
        assert_eq!(&**list.borrow(), &[2,3,4,-1,-1,-1][..]);
    }

    #[test]
    fn test_dorder() {
        let _ = env_logger::init();
        let store = new_store(
            (0..5).map(|i| (23.into(), i.into()))
                .chain((0..5).map(|i| (24.into(), i.into())))
                .chain((0..5).map(|i| (25.into(), i.into())))
                .collect());
        let horizon = HashMap::new();
        let list: Rc<RefCell<Vec<i32>>> = Default::default();
        let mut upcalls: HashMap<_, Box<for<'u, 'o, 'r> Fn(&'u Uuid, &'o OrderIndex, &'r _) -> bool>> = Default::default();
        for i in 23..26 {
            let l = list.clone();
            upcalls.insert(i.into(), Box::new(move |_,_,&v| { l.borrow_mut().push(v);
                true
            }));
        }
        let mut log = FuzzyLog::new(store, horizon, upcalls);
        log.append(24.into(), &4, &[]);
        log.append(23.into(), &2, &[]);
        log.append(25.into(), &3, &[]);
        log.dependent_multiappend(&[23.into()], &[24.into(),25.into()], &-1, &[]);
        log.play_foward(23.into());
        assert_eq!(&**list.borrow(), &[2,4,3,-1,][..]);
    }*/



    struct LogHandle<V> {
        _pd: PhantomData<V>,
        num_snapshots: usize,
        to_log: mpsc::Sender<Message>,
        ready_reads: mpsc::Receiver<Vec<u8>>,
        finished_writes: mpsc::Receiver<(Uuid, Vec<OrderIndex>)>,
        //TODO finished_writes: ..
        curr_entry: Vec<u8>,
    }

    impl<V> Drop for LogHandle<V> {
        fn drop(&mut self) {
            let _ = self.to_log.send(Message::FromClient(Shutdown));
        }
    }

    //TODO I kinda get the feeling that this should send writes directly to the store without
    //     the AsyncLog getting in the middle
    //     Also, I think if I can send assosiated data with the wites I could do multiplexing
    //     over different writers very easily
    impl<V> LogHandle<V>
    where V: Storeable {
        fn snapshot(&mut self, chain: order) {
            self.num_snapshots = self.num_snapshots.saturating_add(1);
            self.to_log.send(Message::FromClient(SnapshotAndPrefetch(chain)))
                .unwrap();
        }

        //TODO return two/three slices?
        fn get_next(&mut self) -> Option<(&V, &[OrderIndex])> {
            if self.num_snapshots == 0 {
                return None
            }

            'recv: loop {
                //TODO use recv_timeout in real version
                self.curr_entry = self.ready_reads.recv().unwrap();
                if self.curr_entry.len() != 0 {
                    break 'recv
                }

                self.num_snapshots = self.num_snapshots.checked_sub(1).unwrap();
                if self.num_snapshots == 0 {
                    return None
                }
            }

            let (val, locs, _) = Entry::<V>::wrap_bytes(&self.curr_entry).val_locs_and_deps();
            Some((val, locs))
        }

        fn append(&mut self, chain: order, data: &V, deps: &[OrderIndex]) -> Vec<OrderIndex> {
            //TODO no-alloc?
            let mut buffer = EntryContents::Data(data, &deps).clone_bytes();
            {
                //TODO I should make a better entry builder
                let e = bytes_as_entry_mut(&mut buffer);
                e.id = Uuid::new_v4();
                e.locs_mut()[0] = (chain, 0.into());
            }
            self.to_log.send(Message::FromClient(PerformAppend(buffer))).unwrap();
            //TODO return buffers here and cache them?
            self.finished_writes.recv().unwrap().1
        }

        fn multiappend(&mut self, chains: &[order], data: &V, deps: &[OrderIndex])
        -> Vec<OrderIndex> {
            //TODO no-alloc?
            assert!(chains.len() > 1);
            let mut locs: Vec<_> = chains.into_iter().map(|&o| (o, 0.into())).collect();
            locs.sort();
            let buffer = EntryContents::Multiput {
                data: data,
                uuid: &Uuid::new_v4(),
                columns: &locs,
                deps: deps,
            }.clone_bytes();
            self.to_log.send(Message::FromClient(PerformAppend(buffer))).unwrap();
            //TODO return buffers here and cache them?
            self.finished_writes.recv().unwrap().1
        }

        //TODO return two vecs
        fn dependent_multiappend(&mut self,
            chains: &[order],
            depends_on: &[order],
            data: &V,
            deps: &[OrderIndex])
        -> Vec<OrderIndex> {
            assert!(depends_on.len() > 1);
            let mut mchains: Vec<_> = chains.into_iter()
                .map(|&c| (c, 0.into()))
                .chain(::std::iter::once((0.into(), 0.into())))
                .chain(depends_on.iter().map(|&c| (c, 0.into())))
                .collect();
            {

                let (chains, deps) = mchains.split_at_mut(chains.len());
                chains.sort();
                deps[1..].sort();
            }
            assert!(mchains[chains.len()] == (0.into(), 0.into()));
            debug_assert!(mchains[..chains.len()].iter().all(|&(o, _)| chains.contains(&o)));
            debug_assert!(mchains[(chains.len() + 1)..]
                .iter().all(|&(o, _)| depends_on.contains(&o)));
            let buffer = EntryContents::Multiput {
                data: data,
                uuid: &Uuid::new_v4(),
                columns: &mchains,
                deps: deps,
            }.clone_bytes();
            self.to_log.send(Message::FromClient(PerformAppend(buffer))).unwrap();
            self.finished_writes.recv().unwrap().1
        }
    }

    #[allow(non_upper_case_globals)]
    const lock_str: &'static str = "0.0.0.0:13389";
    #[allow(non_upper_case_globals)]
    const addr_strs: &'static [&'static str] = &["0.0.0.0:13390", "0.0.0.0:13391"];

    fn new_thread_log<V>(interesting_chains: Vec<order>) -> LogHandle<V> {
        start_servers();

        let to_store_m = Arc::new(Mutex::new(None));
        let tsm = to_store_m.clone();
        let (to_log, from_outside) = mpsc::channel();
        let client = to_log.clone();
        let (ready_reads_s, ready_reads_r) = mpsc::channel();
        let (finished_writes_s, finished_writes_r) = mpsc::channel();
        thread::spawn(move || {
            let mut event_loop = EventLoop::new().unwrap();
            let to_store = event_loop.channel();
            *tsm.lock().unwrap() = Some(to_store);
            let mut store = AsyncTcpStore::new(lock_str.parse().unwrap(),
                addr_strs.into_iter().map(|s| s.parse().unwrap()),
                client, &mut event_loop).expect("");
                event_loop.run(&mut store).expect("should never return");
        });
        let to_store;
        loop {
            let ts = mem::replace(&mut *to_store_m.lock().unwrap(), None);
            if let Some(s) = ts {
                to_store = s;
                break
            }
        }
        thread::spawn(move || {
            let log = ThreadLog::new(to_store, from_outside, ready_reads_s, finished_writes_s,
                interesting_chains.into_iter());
            log.run()
        });



        LogHandle {
            to_log: to_log,
            ready_reads: ready_reads_r,
            finished_writes: finished_writes_r,
            _pd: Default::default(),
            curr_entry: Default::default(),
            num_snapshots: 0,
        }
    }


    fn start_servers()
    {
        use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};
        use std::{thread, iter};

        use servers::tcp::Server;

        static SERVERS_READY: AtomicUsize = ATOMIC_USIZE_INIT;

        for (i, &addr_str) in iter::once(&lock_str).chain(addr_strs.iter()).enumerate() {
            let handle = thread::spawn(move || {

                let addr = addr_str.parse().expect("invalid inet address");
                let mut event_loop = EventLoop::new().unwrap();
                let server = if i == 0 {
                    Server::new(&addr, 0, 1, &mut event_loop)
                }
                else {
                    Server::new(&addr, i as u32 -1, addr_strs.len() as u32,
                        &mut event_loop)
                };
                if let Ok(mut server) = server {
                    SERVERS_READY.fetch_add(1, Ordering::Release);
                    trace!("starting server");
                    event_loop.run(&mut server).expect("server should never stop");
                }
                trace!("server already started");
                return;
            });
            mem::forget(handle);
        }

        while SERVERS_READY.load(Ordering::Acquire) < addr_strs.len() + 1 {}
    }
}
