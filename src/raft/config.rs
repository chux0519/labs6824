use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::{sync::mpsc::unbounded, Future, Stream};
use labrpc;

use raft;
use raft::errors::Error;
use raft::persister::*;
use rand::Rng;

static ID: AtomicUsize = AtomicUsize::new(0);

fn uniqstring() -> String {
    format!("{}", ID.fetch_add(1, Ordering::Relaxed))
}

/// A log entry.
#[derive(Clone, PartialEq, Message)]
pub struct Entry {
    #[prost(int64, tag = "100")]
    pub x: i64,
}

struct Storage {
    // copy of each server's committed entries
    logs: Vec<HashMap<u64, Entry>>,
    max_index: u64,
    max_index0: u64,
}

pub struct Config {
    net: labrpc::Network,
    n: usize,
    pub rafts: Vec<Option<raft::Node>>,
    // applyErr:  []string // from apply channel readers
    // whether each server is on the net
    connected: Vec<bool>,
    saved: Vec<Arc<SimplePersister>>,
    // the port file names each sends to
    endnames: Vec<Vec<String>>,

    storage: Arc<Mutex<Storage>>,

    // time at which make_config() was called
    start: Instant,

    // begin()/end() statistics

    // time at which test_test.go called cfg.begin()
    t0: Instant,
    // rpc_total() at start of test
    rpcs0: usize,
    // number of agreements
    cmds0: usize,
}

impl Config {
    pub fn new(n: usize, unreliable: bool) -> Config {
        let net = labrpc::Network::new();
        net.set_reliable(!unreliable);
        net.set_long_delays(true);
        let storage = Storage {
            logs: vec![HashMap::new(); n],
            max_index: 0,
            max_index0: 0,
        };
        let mut cfg = Config {
            net,
            n,
            rafts: vec![None; n],
            connected: vec![true; n],
            saved: (0..n).map(|_| Arc::new(SimplePersister::new())).collect(),
            endnames: vec![vec![String::new(); n]; n],
            storage: Arc::new(Mutex::new(storage)),

            start: Instant::now(),
            t0: Instant::now(),
            rpcs0: 0,
            cmds0: 0,
        };

        for i in 0..n {
            cfg.start1(i);
        }

        for i in 0..n {
            cfg.connect(i);
        }

        cfg
    }

    fn rpc_count(&self, server: usize) -> usize {
        self.net.count(&format!("{}", server))
    }

    fn rpc_total(&self) -> usize {
        self.net.total_count()
    }

    // check that there's exactly one leader.
    // try a few times in case re-elections are needed.
    pub fn check_one_leader(&self) -> usize {
        let mut random = rand::thread_rng();
        let mut leaders = HashMap::new();
        for iters in 0..10 {
            let ms = 450 + (random.gen::<u64>() % 100);
            thread::sleep(Duration::from_millis(ms));

            for (i, connected) in self.connected.iter().enumerate() {
                if *connected {
                    let term = self.rafts[i].as_ref().unwrap().term();
                    let is_leader = self.rafts[i].as_ref().unwrap().is_leader();
                    if is_leader {
                        leaders.entry(term).or_insert_with(Vec::new).push(i);
                    }
                }
            }

            let mut last_term_with_leader = 0;
            for (term, leaders) in &leaders {
                if leaders.len() > 1 {
                    panic!("term {} has {:?} (>1) leaders", term, leaders);
                }
                if *term > last_term_with_leader {
                    last_term_with_leader = *term;
                }
            }

            if !leaders.is_empty() {
                return leaders[&last_term_with_leader][0];
            }
        }

        panic!("expected one leader, got none")
    }

    // check that everyone agrees on the term.
    pub fn check_terms(&self) -> u64 {
        let mut term = 0;
        for (i, connected) in self.connected.iter().enumerate() {
            if *connected {
                let xterm = self.rafts[i].as_ref().unwrap().term();
                if term == 0 {
                    term = xterm;
                } else if term != xterm {
                    panic!("servers disagree on term");
                }
            }
        }
        term
    }

    // check that there's no leader
    pub fn check_no_leader(&self) {
        for (i, connected) in self.connected.iter().enumerate() {
            if *connected {
                let is_leader = self.rafts[i].as_ref().unwrap().is_leader();
                if is_leader {
                    panic!("expected no leader, but {} claims to be leader", i);
                }
            }
        }
    }

    fn check_timeout(&self) {
        // enforce a two minute real-time limit on each test
        if Instant::now() - self.start > Duration::from_secs(120) {
            panic!("test took longer than 120 seconds");
        }
    }

    // how many servers think a log entry is committed?
    pub fn n_committed(&self, index: u64) -> (usize, Option<Entry>) {
        let mut count = 0;
        let mut cmd = None;
        let s = self.storage.lock().unwrap();
        for (i, raft) in self.rafts.iter().enumerate() {
            let cmd1 = s.logs[i].get(&index).cloned();

            if cmd1.is_some() {
                if count > 0 && cmd != cmd1 {
                    panic!(
                        "committed values do not match: index {:?}, {:?}, {:?}",
                        index, cmd, cmd1
                    );
                }
                count += 1;
                cmd = cmd1;
            }
        }
        (count, cmd)
    }

    // wait for at least n servers to commit.
    // but don't wait forever.
    pub fn wait(&self, index: u64, n: usize, start_term: Option<u64>) -> Option<Entry> {
        let mut to = Duration::from_millis(10);
        for iters in 0..30 {
            let (nd, _) = self.n_committed(index);
            if nd >= n {
                break;
            }
            thread::sleep(to);
            if to < Duration::from_secs(1) {
                to *= 2;
            }
            if let Some(start_term) = start_term {
                for r in &self.rafts {
                    if let Some(rf) = r {
                        let term = rf.term();
                        if term > start_term {
                            // someone has moved on
                            // can no longer guarantee that we'll "win"
                            return None;
                        }
                    }
                }
            }
        }
        let (nd, cmd) = self.n_committed(index);
        if nd < n {
            panic!("only {} decided for index {}; wanted {}", nd, index, n);
        }
        cmd
    }

    // do a complete agreement.
    // it might choose the wrong leader initially,
    // and have to re-submit after giving up.
    // entirely gives up after about 10 seconds.
    // indirectly checks that the servers agree on the
    // same value, since n_committed() checks this,
    // as do the threads that read from applyCh.
    // returns index.
    // if retry==true, may submit the command multiple
    // times, in case a leader fails just after Start().
    // if retry==false, calls Start() only once, in order
    // to simplify the early Lab 2B tests.
    pub fn one(&self, cmd: Entry, expected_servers: usize, retry: bool) -> u64 {
        let t0 = Instant::now();
        let mut starts = 0;
        while t0.elapsed() < Duration::from_secs(10) {
            // try all the servers, maybe one is the leader.
            let mut index = None;
            for si in 0..self.n {
                starts = (starts + 1) % self.n;
                if self.connected[starts] {
                    if let Some(ref rf) = &self.rafts[starts] {
                        match rf.start(&cmd) {
                            Ok((index1, _)) => {
                                index = Some(index1);
                                break;
                            }
                            Err(e) => debug!("start cmd {:?} failed: {:?}", cmd, e),
                        }
                    }
                }
            }

            if let Some(index) = index {
                // somebody claimed to be the leader and to have
                // submitted our command; wait a while for agreement.
                let t1 = Instant::now();
                while t1.elapsed() < Duration::from_secs(2) {
                    let (nd, cmd1) = self.n_committed(index);
                    if nd > 0 && nd >= expected_servers {
                        // committed
                        if let Some(cmd2) = cmd1 {
                            if cmd2 == cmd {
                                // and it was the command we submitted.
                                return index;
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                if !retry {
                    panic!("one({:?}) failed to reach agreement", cmd);
                }
            } else {
                thread::sleep(Duration::from_millis(50));
            }
        }
        panic!("one({:?}) failed to reach agreement", cmd);
    }

    // start a Test.
    // print the Test message.
    // e.g. cfg.begin("Test (2B): RPC counts aren't too high")
    pub fn begin(&mut self, description: &str) {
        info!("{} ...", description);
        self.t0 = Instant::now();
        self.rpcs0 = self.rpc_total();
        self.cmds0 = 0;

        let mut s = self.storage.lock().unwrap();
        s.max_index0 = s.max_index;
    }

    // end a Test -- the fact that we got here means there
    // was no failure.
    // print the Passed message,
    // and some performance numbers.
    pub fn end(&mut self) {
        self.check_timeout();

        // real time
        let t = Instant::now() - self.t0;
        // number of Raft peers
        let npeers = self.n;
        // number of RPC sends
        let nrpc = self.rpc_total() - self.rpcs0;

        // number of Raft agreements reported
        let s = self.storage.lock().unwrap();
        let ncmds = s.max_index - s.max_index0;

        info!("  ... Passed --");
        info!("  {:?}  {} {} {}", t, npeers, nrpc, ncmds);
    }

    /// start or re-start a Raft.
    /// if one already exists, "kill" it first.
    /// allocate new outgoing port file names, and a new
    /// state persister, to isolate previous instance of
    /// this server. since we cannot really kill it.
    fn start1(&mut self, i: usize) {
        self.crash1(i);

        // a fresh set of outgoing ClientEnd names.
        // so that old crashed instance's ClientEnds can't send.
        self.endnames[i] = vec![String::new(); self.n];
        for j in 0..self.n {
            self.endnames[i][j] = uniqstring();
        }

        // a fresh set of ClientEnds.
        let mut clients = Vec::with_capacity(self.n);
        for (j, name) in self.endnames[i].iter().enumerate() {
            let cli = self.net.create_client(name.to_string());
            let client = raft::service::RaftClient::new(cli);
            clients.push(client);
            self.net.connect(name, &format!("{}", j));
        }

        // listen to messages from Raft indicating newly committed messages.
        let (tx, apply_ch) = unbounded();
        let storage = self.storage.clone();
        let apply = apply_ch
            .for_each(move |cmd: raft::ApplyMsg| {
                if !cmd.command_valid {
                    // ignore other types of ApplyMsg
                    return Ok(());
                }
                match labcodec::decode(&cmd.command) {
                    Ok(entry) => {
                        let mut s = storage.lock().unwrap();
                        for (j, log) in s.logs.iter().enumerate() {
                            if let Some(old) = log.get(&cmd.command_index) {
                                if *old != entry {
                                    // some server has already committed a different value for this entry!
                                    panic!(
                                        "commit index={:?} server={:?} {:?} != server={:?} {:?}",
                                        cmd.command_index, i, entry, j, old
                                    );
                                }
                            }
                        }
                        if cmd.command_index > 1 {
                            let log = s.logs.get_mut(i - 1);
                            if let Some(mut log) = log {
                                log.insert(cmd.command_index, entry);
                            } else {
                                panic!("server {} apply out of order {}", i, cmd.command_index);
                            }
                        }
                        if cmd.command_index > s.max_index {
                            s.max_index = cmd.command_index;
                        }
                    }
                    Err(e) => {
                        panic!("committed command is not an entry {:?}", e);
                    }
                }
                Ok(())
            })
            .map_err(move |e| debug!("raft {} apply stopped: {:?}", i, e));
        self.net.spawn(apply);

        let rf = raft::Raft::new(clients, i, Box::new(self.saved[i].clone()), tx);
        let node = raft::Node::new(rf);
        self.rafts[i] = Some(node.clone());

        let mut builder = labrpc::ServerBuilder::new(format!("{}", i));
        raft::add_raft_service(node, &mut builder).unwrap();
        let srv = builder.build();
        self.net.add_server(srv.clone());
    }

    /// shut down a Raft server but save its persistent state.
    fn crash1(&mut self, i: usize) {
        self.disconnect(i);
        // disable client connections to the server.
        self.net.delete_server(&format!("{}", i));

        // a fresh persister, in case old instance
        // continues to update the Persister.
        // but copy old persister's content so that we always
        // pass Make() the last persisted state.
        let p = SimplePersister::new();
        p.save_raft_state(self.saved[i].raft_state());
        self.saved[i] = Arc::new(p);

        if let Some(rf) = self.rafts[i].take() {
            rf.kill();
        }
    }

    /// detach server i from the net.
    pub fn disconnect(&mut self, i: usize) {
        debug!("disconnect({})", i);

        self.connected[i] = false;

        // outgoing ClientEnds
        for endname in &self.endnames[i] {
            self.net.enable(endname, false);
        }

        // incoming ClientEnds
        for names in &self.endnames {
            let endname = &names[i];
            self.net.enable(endname, false);
        }
    }

    // attach server i to the net.
    pub fn connect(&mut self, i: usize) {
        debug!("connect({})", i);

        self.connected[i] = true;

        // outgoing ClientEnds
        for (j, connected) in self.connected.iter().enumerate() {
            if *connected {
                let endname = &self.endnames[i][j];
                self.net.enable(endname, true);
            }
        }

        // incoming ClientEnds
        for (j, connected) in self.connected.iter().enumerate() {
            if *connected {
                let endname = &self.endnames[j][i];
                self.net.enable(endname, true);
            }
        }
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        for r in &self.rafts {
            if let Some(rf) = r {
                rf.kill();
            }
        }
        self.check_timeout();
    }
}
