
//TODO use faster HashMap, HashSet
use std::{self, iter, mem};
use std::collections::VecDeque;
use std::collections::hash_map;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc;
use std::u32;

use mio;

use packets::*;
use async::store::AsyncStoreClient;
use self::FromStore::*;
use self::FromClient::*;

use hash::HashMap;

use self::per_color::{PerColor, IsRead, ReadHandle};

pub mod log_handle;
mod per_color;

#[cfg(test)]
mod tests;

const MAX_PREFETCH: u32 = 8;

type ChainEntry = Rc<Vec<u8>>;

pub struct ThreadLog {
    to_store: mio::channel::Sender<Vec<u8>>, //TODO send WriteState or other enum?
    from_outside: mpsc::Receiver<Message>, //TODO should this be per-chain?
    blockers: HashMap<OrderIndex, Vec<ChainEntry>>,
    blocked_multiappends: HashMap<Uuid, MultiSearchState>,
    per_chains: HashMap<order, PerColor>,
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
    ReturnBuffer(Vec<u8>),
    Shutdown,
}

enum MultiSearch {
    Finished(Vec<u8>),
    InProgress,
    EarlySentinel,
    BeyondHorizon(Vec<u8>),
    Repeat,
    //MultiSearch::FirstPart(),
}

//TODO no-alloc
struct BufferCache {
    vec_cache: VecDeque<Vec<u8>>,
    //     rc_cache: VecDeque<Rc<Vec<u8>>>,
    //     alloced: usize,
    //     avg_alloced: usize,
}

impl ThreadLog {

    //TODO
    pub fn new<I>(to_store: mio::channel::Sender<Vec<u8>>,
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
            per_chains: interesting_chains.into_iter().map(|c| (c, PerColor::interesting(c))).collect(),
            to_return: Default::default(),
            no_longer_blocked: Default::default(),
            cache: BufferCache::new(),
            chains_currently_being_read: Rc::new(ReadHandle),
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
                trace!("FUZZY snapshot");
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                //FIXME
                if chain != 0.into() {
                    self.fetch_snapshot(chain);
                    self.prefetch(chain);
                }
                else {
                    let chains: Vec<_> = self.per_chains.iter()
                        .filter(|pc| pc.1.is_interesting)
                        .map(|pc| pc.0.clone()).collect();
                    for chain in chains {
                        self.fetch_snapshot(chain);
                        self.prefetch(chain);
                    }
                }
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
            ReturnBuffer(buffer) => {
                self.cache.cache_buffer(buffer);
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
            pc.increment_outstanding_snapshots(&self.chains_currently_being_read);
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
                        s.decrement_outstanding_reads();
                    });
                }
                else {
                    let unblocked = self.per_chains.get_mut(&read_loc.0).and_then(|s| {
                        let e = bytes_as_entry(&msg);
                        assert_eq!(e.locs()[0].1, u32::MAX.into());
                        debug_assert!(!e.kind.contains(EntryKind::ReadSuccess));
                        let new_horizon = e.dependencies()[0].1;
                        trace!("FUZZY try update horizon to {:?}", (read_loc.0, new_horizon));
                        s.give_new_snapshot(new_horizon)
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
                //TODO check that read is needed?
                //TODO no-alloc?
                self.per_chains.get_mut(&read_loc.0).map(|s|
                    s.decrement_outstanding_reads());
                let packet = Rc::new(msg);
                self.add_blockers(&packet);
                self.try_returning_at(read_loc, packet);
            }
            layout @ EntryLayout::Multiput | layout @ EntryLayout::Sentinel => {
                trace!("FUZZY read is multi");
                debug_assert!(kind.contains(EntryKind::ReadSuccess));
                self.per_chains.get_mut(&read_loc.0).map(|s|
                    s.decrement_outstanding_reads());
                let is_sentinel = layout == EntryLayout::Sentinel;
                let search_status = self.update_multi_part_read(read_loc, msg, is_sentinel);
                match search_status {
                    MultiSearch::InProgress | MultiSearch::EarlySentinel => {}
                    MultiSearch::BeyondHorizon(..) => {
                        //TODO better ooo reads
                        self.per_chains.entry(read_loc.0)
                            .or_insert_with(|| PerColor::new(read_loc.0))
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
                    MultiSearch::Repeat => {}
                }
            }

            EntryLayout::Lock => unreachable!(),
        }

        let finished_server = self.continue_fetch_if_needed(read_loc.0);
        if finished_server {
            trace!("FUZZY finished reading {:?}", read_loc.0);

            self.per_chains.get_mut(&read_loc.0).map(|pc| {
                debug_assert!(pc.is_finished());
                trace!("FUZZY chain {:?} is finished", pc.chain);
                pc.set_finished_reading();
            });
            if self.finshed_reading() {
                trace!("FUZZY finished reading all chains");
                //FIXME add is_snapshoting to PerColor so this doesn't race?
                trace!("FUZZY finished reading");
                //TODO do we need a better system?
                let num_completeds = mem::replace(&mut self.num_snapshots, 0);
                //assert!(num_completeds > 0);
                for _ in 0..num_completeds {
                    let _ = self.ready_reads.send(vec![]);
                }
            }
        }
        else {
            //#[cfg(debug_assertions)]
            //self.per_chains.get(&read_loc.0).map(|pc| {
            //    trace!("chain {:?} not finished, " pc.outstanding_reads, pc.last_returned)
            //});
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
        for &OrderIndex(chain, index) in deps {
            let blocker_already_returned = self.per_chains.get_mut(&chain)
                .expect("read uninteresting chain")
                .has_returned(index);
            if !blocker_already_returned {
                trace!("FUZZY read @ {:?} blocked on {:?}", locs, (chain, index));
                //TODO no-alloc?
                let blocked = self.blockers.entry(OrderIndex(chain, index))
                    .or_insert_with(Vec::new);
                blocked.push(packet.clone());
            } else {
                trace!("FUZZY read @ {:?} need not wait for {:?}", locs, (chain, index));
            }
        }
        for &loc in locs {
            if loc.0 == order::from(0) { continue }
            let (is_next_in_chain, needs_to_be_returned) = {
                let pc = self.per_chains.get(&loc.0).expect("fetching uninteresting chain");
                (pc.next_return_is(loc.1), !pc.has_returned(loc.1))
            };
            if !is_next_in_chain && needs_to_be_returned {
                self.enqueue_packet(loc, packet.clone());
            }
        }
    }

    fn fetch_blockers_if_needed(&mut self, packet: &ChainEntry) {
        //TODO num_to_fetch
        //FIXME only do if below last_snapshot?
        let deps = bytes_as_entry(packet).dependencies();
        for &OrderIndex(chain, index) in deps {
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
                .filter(|&&OrderIndex(o, i)| o != order::from(0))
                .count();
            trace!("FUZZY multi part read {:?} @ {:?}, {:?} pieces", id, locs, num_pieces);
            (id, num_pieces)
        };

        //TODO this should never really occur...
        if num_pieces == 1 {
            return MultiSearch::Finished(msg)
        }

        let is_later_piece = self.blocked_multiappends.contains_key(&id);
        let mut omsg = None;
        if !is_later_piece && !is_sentinel {
            {
                let pc = &self.per_chains[&read_loc.0];
                //FIXME I'm not sure if this is right
                if !pc.is_within_snapshot(read_loc.1) {
                    trace!("FUZZY read multi too early @ {:?}", read_loc);
                    return MultiSearch::BeyondHorizon(msg)
                }

                if pc.has_returned(read_loc.1) {
                    trace!("FUZZY duplicate multi @ {:?}", read_loc);
                    return MultiSearch::BeyondHorizon(msg)
                }
            }

            let mut pieces_remaining = num_pieces;
            trace!("FUZZY first part of multi part read");
            for &mut OrderIndex(o, ref mut i) in bytes_as_entry_mut(&mut msg).locs_mut() {
                if o != order::from(0) {
                    trace!("FUZZY fetching multi part @ {:?}?", (o, *i));
                    let early_sentinel = self.fetch_multi_parts(&id, o, *i);
                    if let Some(loc) = early_sentinel {
                        trace!("FUZZY no fetch @ {:?} sentinel already found", (o, *i));
                        assert!(loc != entry::from(0));
                        *i = loc;
                        pieces_remaining -= 1
                    } else if *i != entry::from(0) {
                        trace!("FUZZY multi shortcircuit @ {:?}", (o, *i));
                        pieces_remaining -= 1
                    }
                } else {
                    trace!("FUZZY no need to fetch multi part @ {:?}", (o, *i));
                }
            }

            if pieces_remaining == 0 {
                trace!("FUZZY all sentinels had already been found for {:?}", read_loc);
                return MultiSearch::Finished(msg)
            }

            trace!("FUZZY {:?} waiting for {:?} pieces", read_loc, pieces_remaining);
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
        else { omsg = Some(msg); trace!("FUZZY later part of multi part read"); }

        debug_assert!(self.per_chains[&read_loc.0].is_within_snapshot(read_loc.1));

        let was_blind_search;
        let finished = {
            if let hash_map::Entry::Occupied(mut found) = self.blocked_multiappends.entry(id) {
                let finished = {
                    let multi = found.get_mut();
                    //FIXME ensure this only happens if debug assertions
                    if let (Some(msg), false) = (omsg, is_sentinel) {
                        unsafe { debug_assert_eq!(data_bytes(&multi.val), data_bytes(&msg)) }
                    }
                    let loc_ptr = bytes_as_entry_mut(&mut multi.val)
                        .locs_mut().into_iter()
                        .find(|&&mut OrderIndex(o, _)| o == read_loc.0)
                        .unwrap();
                    //FIXME
                    was_blind_search = loc_ptr.1 == entry::from(0);
                    if !was_blind_search {
                        debug_assert_eq!(*loc_ptr, read_loc)
                    } else {
                        multi.pieces_remaining -= 1;
                        trace!("FUZZY multi pieces remaining {:?}", multi.pieces_remaining);
                        *loc_ptr = read_loc;
                    }

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
            let pc = self.per_chains.entry(read_loc.0)
                .or_insert_with(|| PerColor::new(read_loc.0));
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
            let pc = self.per_chains.entry(chain)
                .or_insert_with(|| PerColor::new(chain));

            let early_sentinel = pc.take_early_sentinel(&id);
            let potential_new_horizon = match early_sentinel {
                Some(loc) => loc,
                None => index,
            };

            //perform a non blind search if possible
            //TODO less than ideal with new lock scheme
            //     lock index is always below color index, starting with a non-blind read
            //     based on the lock number should be balid, if a bit conservative
            //     this would require some way to fall back to a blind read,
            //     if the horizon was reached before the multi found
            if index != entry::from(0) /* && !pc.is_within_snapshot(index) */ {
                trace!("RRRRR non-blind search {:?} {:?}", chain, index);
                let unblocked = pc.update_horizon(potential_new_horizon);
                pc.mark_as_already_fetched(index);
                (unblocked, early_sentinel)
            } else if early_sentinel.is_some() {
                trace!("RRRRR already found {:?} {:?}", chain, early_sentinel);
                //FIXME How does this interact with cached reads?
                (None, early_sentinel)
            } else {
                trace!("RRRRR blind search {:?}", chain);
                pc.increment_multi_search(&self.chains_currently_being_read);
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
            let pc = self.per_chains.entry(chain).or_insert_with(|| PerColor::new(chain));
            let num_to_fetch = pc.num_to_fetch();
            //TODO should fetch == number of multis searching for
            if num_to_fetch > 0 {
                trace!("FUZZY {:?} needs {:?} additional reads", chain, num_to_fetch);
                (num_to_fetch, None)
            } else if pc.has_more_multi_search_than_outstanding_reads() {
                trace!("FUZZY {:?} updating horizon due to multi search", chain);
                (1, pc.increment_horizon())
            }
            else {
                (0, None)
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
        debug_assert!(
            !self.per_chains[&loc.0].next_return_is(loc.1)
            && !self.per_chains[&loc.0].has_returned(loc.1),
            //self.per_chains.get(&loc.0).unwrap().last_returned_to_client
            //< loc.1 - 1,
            "tried to enqueue non enqueable entry {:?};",// last returned {:?}",
            loc.1 - 1,
            //self.per_chains.get(&loc.0).unwrap().last_returned_to_client,
        );
        let blocked_on = OrderIndex(loc.0, loc.1 - 1);
        trace!("FUZZY read @ {:?} blocked on prior {:?}", loc, blocked_on);
        //TODO no-alloc?
        let blocked = self.blockers.entry(blocked_on).or_insert_with(Vec::new);
        blocked.push(packet.clone());
    }

    fn return_entry_at(&mut self, loc: OrderIndex, val: Vec<u8>) -> bool {
        debug_assert!(bytes_as_entry(&val).locs()[0] == loc);
        debug_assert!(bytes_as_entry(&val).locs().len() == 1);
        trace!("FUZZY trying to return read @ {:?}", loc);
        let OrderIndex(o, i) = loc;

        let is_interesting = {
            let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");

            if pc.has_returned(i) {
                return false
            }

            if !pc.is_within_snapshot(i) {
                trace!("FUZZY blocking read @ {:?}, waiting for snapshot", loc);
                pc.block_on_snapshot(val);
                return false
            }

            trace!("QQQQQ setting returned {:?}", (o, i));
            pc.set_returned(i);
            pc.is_interesting
        };
        trace!("FUZZY returning read @ {:?}", loc);
        if is_interesting {
            //FIXME first_buffered?
            self.ready_reads.send(val).expect("client hung up");
        }
        true
    }

    ///returns None if return stalled Some(Locations which are now unblocked>) if return
    ///        succeeded
    //TODO it may make sense to change these funtions to add the returned messages to an
    //     internal ring which can be used to discover the unblocked entries before the
    //     messages are flushed to the client, as this would remove the intermidate allocation
    //     and it may be a bit nicer
    fn return_entry(&mut self, val: Vec<u8>) -> Option<Vec<OrderIndex>> {
        let (locs, is_interesting) = {
            let mut should_block_on = None;
            {
                let locs = bytes_as_entry(&val).locs();
                trace!("FUZZY trying to return read from {:?}", locs);
                for &OrderIndex(o, i) in locs.into_iter() {
                    if o == order::from(0) { continue }
                    let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                    if pc.has_returned(i) { return None }
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
            let mut is_interesting = false;
            let locs = bytes_as_entry(&val).locs();
            for &OrderIndex(o, i) in locs.into_iter() {
                if o == order::from(0) { continue }
                trace!("QQQQ setting returned {:?}", (o, i));
                let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                debug_assert!(pc.is_within_snapshot(i));
                pc.set_returned(i);
                is_interesting |= pc.is_interesting;
            }
            //TODO no-alloc
            //     a better solution might be to have this function push onto a temporary
            //     VecDeque who's head is used to unblock further entries, and is then sent
            //     to the client
            (locs.to_vec(), is_interesting)
        };
        trace!("FUZZY returning read @ {:?}", locs);
        if is_interesting {
            //FIXME first_buffered?
            self.ready_reads.send(val).expect("client hung up");
        }
        Some(locs)
    }

    fn fetch_next(&mut self, chain: order) {
        let next = {
            let per_chain = &mut self.per_chains.get_mut(&chain)
                .expect("fetching uninteresting chain");
            //assert!(per_chain.last_read_sent_to_server < per_chain.last_snapshot,
            //    "last_read_sent_to_server {:?} >= {:?} last_snapshot @ fetch_next",
            //    per_chain.last_read_sent_to_server, per_chain.last_snapshot,
            //);
            per_chain.increment_fetch(&self.chains_currently_being_read)
        };
        let packet = self.make_read_packet(chain, next);

        self.to_store.send(packet).expect("store hung up");
    }

    fn make_read_packet(&mut self, chain: order, index: entry) -> Vec<u8> {
        let mut buffer = self.cache.alloc();
        {
            let e = EntryContents::Data(&(), &[]).fill_vec(&mut buffer);
            e.kind = EntryKind::Read;
            e.locs_mut()[0] = OrderIndex(chain, index);
            debug_assert_eq!(e.data_bytes, 0);
            debug_assert_eq!(e.dependency_bytes, 0);
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
                assert_eq!(pc.is_finished(), !pc.has_read_state());
                if !pc.is_finished() {
                    currently_being_read += 1
                }
                //still_reading |= pc.has_outstanding_reads()
            }
            // !still_reading == (self.servers_currently_being_read == 0)
            if finished != (currently_being_read == 0) {
                panic!("currently_being_read == {:?} @ finish {:?}",
                currently_being_read, finished);
            }
            currently_being_read == 0
        }, finished);

        finished
    }

    fn server_is_finished(&self, chain: order) -> bool {
        let pc = &self.per_chains[&chain];
        assert!(!(!pc.has_outstanding_reads() && pc.has_pending_reads_reqs()));
        assert!(!(pc.is_searching_for_multi() && !pc.has_outstanding_reads()));
        pc.is_finished()
    }
}

impl BufferCache {
    fn new() -> Self {
        BufferCache{
            vec_cache: VecDeque::new()
        }
    }

    fn alloc(&mut self) -> Vec<u8> {
        self.vec_cache.pop_front().unwrap_or(Vec::new())
    }

    fn cache_buffer(&mut self, mut buffer: Vec<u8>) {
        //TODO
        if self.vec_cache.len() < 100 {
            buffer.clear();
            self.vec_cache.push_front(buffer)
        }
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
