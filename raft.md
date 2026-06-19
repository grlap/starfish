# Raft Consensus Protocol — Design & Verification Plan

## Overview

Implement the Raft consensus protocol in Rust on top of the starfish-reactor runtime,
verified at multiple levels using formal methods and deterministic simulation.

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Layer 3: Full Integration (SimulatedIOManager) │
│  - Tests the complete stack end-to-end          │
│  - Fault injection, million-seed fuzzing        │
│  - Catches: async bugs, IO edge cases           │
├─────────────────────────────────────────────────┤
│  Layer 2: Model Checking (Stateright)           │
│  - Exhaustive state exploration                 │
│  - Proves safety properties hold                │
│  - Catches: protocol logic bugs                 │
├─────────────────────────────────────────────────┤
│  Layer 1: Pure Raft State Machine               │
│  - fn step(&mut self, input) -> Vec<output>     │
│  - No IO, no async, no reactor dependency       │
│  - Shared by ALL layers above                   │
└─────────────────────────────────────────────────┘
```

---

## Layer 1: Pure Raft State Machine

The core implementation. No IO, no async — just a pure state machine that can be
driven by any harness (tests, Stateright, simulated IO, or production reactor).

### State

```rust
pub struct RaftNode<C: StateMachine> {
    // Identity
    id: NodeId,
    cluster: Vec<NodeId>,

    // Persistent state (survives restarts)
    current_term: u64,
    voted_for: Option<NodeId>,
    log: Vec<LogEntry<C::Command>>,

    // Volatile state
    role: Role,
    commit_index: u64,
    last_applied: u64,

    // Leader-only volatile state
    next_index: HashMap<NodeId, u64>,
    match_index: HashMap<NodeId, u64>,

    // Election
    election_ticks_remaining: u32,
    votes_received: HashSet<NodeId>,

    // Application state machine
    state_machine: C,
}

pub enum Role {
    Follower,
    Candidate,
    Leader,
}

pub struct LogEntry<Cmd> {
    term: u64,
    index: u64,
    command: Cmd,
}
```

### Input / Output

```rust
pub enum RaftInput<Cmd> {
    // RPCs from peers
    AppendEntries {
        term: u64,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: u64,
        entries: Vec<LogEntry<Cmd>>,
        leader_commit: u64,
    },
    AppendEntriesResponse {
        term: u64,
        success: bool,
        match_index: u64,
    },
    RequestVote {
        term: u64,
        candidate_id: NodeId,
        last_log_index: u64,
        last_log_term: u64,
    },
    RequestVoteResponse {
        term: u64,
        vote_granted: bool,
    },

    // External triggers
    ClientRequest { command: Cmd },
    Tick,
}

pub enum RaftOutput<Cmd> {
    SendMessage { to: NodeId, msg: RaftInput<Cmd> },
    ApplyToStateMachine { index: u64, command: Cmd },
    PersistState,
    ResetElectionTimer,
    ClientResponse { index: u64, success: bool },
}
```

### Core Logic

```rust
impl<C: StateMachine> RaftNode<C> {
    /// Pure state transition — the heart of the implementation.
    /// No IO, no side effects beyond mutating self.
    pub fn step(&mut self, from: NodeId, input: RaftInput<C::Command>) -> Vec<RaftOutput<C::Command>> {
        match input {
            RaftInput::Tick => self.on_tick(),
            RaftInput::AppendEntries { .. } => self.on_append_entries(from, ...),
            RaftInput::AppendEntriesResponse { .. } => self.on_append_entries_response(from, ...),
            RaftInput::RequestVote { .. } => self.on_request_vote(from, ...),
            RaftInput::RequestVoteResponse { .. } => self.on_request_vote_response(from, ...),
            RaftInput::ClientRequest { command } => self.on_client_request(command),
        }
    }
}
```

### StateMachine Trait

```rust
pub trait StateMachine: Clone + Default {
    type Command: Clone + Debug + PartialEq;
    type Response;

    fn apply(&mut self, command: &Self::Command) -> Self::Response;
}
```

---

## Layer 2: Stateright Model Checking

Wrap the pure state machine in Stateright's `Actor` trait for exhaustive state exploration.

### Actor Adapter

```rust
impl Actor for RaftActor {
    type Msg = RaftInput<TestCommand>;
    type State = RaftNode<TestStateMachine>;
    type Timer = RaftTimer;

    fn on_msg(&self, id: Id, state: &mut Self::State,
              src: Id, msg: Self::Msg, o: &mut Out<Self>) {
        let outputs = state.step(src.into(), msg);
        for output in outputs {
            match output {
                RaftOutput::SendMessage { to, msg } => o.send(to.into(), msg),
                RaftOutput::ResetElectionTimer => {
                    o.set_timer(RaftTimer::Election, self.election_timeout);
                }
                RaftOutput::ApplyToStateMachine { .. } => { /* recorded in state */ }
                _ => {}
            }
        }
    }

    fn on_timeout(&self, id: Id, state: &mut Self::State,
                  timer: &Self::Timer, o: &mut Out<Self>) {
        match timer {
            RaftTimer::Election => {
                let outputs = state.step(id.into(), RaftInput::Tick);
                // handle outputs...
            }
            RaftTimer::Heartbeat => { /* leader sends AppendEntries */ }
        }
    }
}
```

### Safety Properties to Check

```rust
fn properties() -> Vec<Property<RaftModel>> {
    vec![
        // Election Safety: at most one leader per term
        Property::always("election_safety", |model, state| {
            for term in all_terms(state) {
                let leaders: Vec<_> = state.nodes.iter()
                    .filter(|n| n.role == Role::Leader && n.current_term == term)
                    .collect();
                if leaders.len() > 1 { return false; }
            }
            true
        }),

        // Log Matching: if two logs contain an entry with the same
        // index and term, the logs are identical up to that index
        Property::always("log_matching", |model, state| {
            for (i, ni) in state.nodes.iter().enumerate() {
                for nj in state.nodes.iter().skip(i + 1) {
                    for entry_i in &ni.log {
                        for entry_j in &nj.log {
                            if entry_i.index == entry_j.index
                                && entry_i.term == entry_j.term
                                && entry_i.command != entry_j.command
                            {
                                return false;
                            }
                        }
                    }
                }
            }
            true
        }),

        // Leader Completeness: if an entry is committed in a given term,
        // that entry is present in the logs of all leaders for higher terms
        Property::always("leader_completeness", |model, state| {
            // ... check committed entries appear in future leaders
            true
        }),

        // State Machine Safety: no two nodes apply different commands
        // at the same log index
        Property::always("state_machine_safety", |model, state| {
            // ... compare applied entries across nodes
            true
        }),
    ]
}
```

### Running the Checker

```rust
#[test]
fn model_check_raft_3_nodes() {
    RaftModelCfg { cluster_size: 3 }
        .into_model()
        .checker()
        .threads(num_cpus::get())
        .spawn_bfs()
        .join()
        .assert_properties();
}
```

---

## Layer 3: Simulated IOManager (Deterministic Simulation Testing)

Leverages the existing `IOManager` trait to replace real kernel IO with
a deterministic, fault-injectable simulation.

### SimulatedIOManager

```rust
pub struct SimulatedIOManager {
    /// Pending IO operations waiting for simulated completion
    pending: VecDeque<PendingOp>,
    /// Completed operations ready to return
    completed: VecDeque<(FutureRuntime, *const IOWaitFuture)>,
    /// Reference to shared simulation bus
    bus: Rc<RefCell<SimulationBus>>,
    /// This node's identity
    node_id: NodeId,
}

impl IOManager for SimulatedIOManager {
    fn register_io_wait(&mut self, ptr: *const IOWaitFuture) -> Result<(), io::Error> {
        // Decode the IO operation from the future
        // Enqueue into simulation bus instead of submitting to kernel
        self.bus.borrow_mut().enqueue(self.node_id, ptr);
        Ok(())
    }

    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        self.completed.pop_front()
    }

    fn has_active_io(&self) -> bool {
        !self.pending.is_empty() || !self.completed.is_empty()
    }

    fn cancel_io_wait(&mut self, ptr: *const IOWaitFuture) -> Result<(), io::Error> {
        self.bus.borrow_mut().cancel(self.node_id, ptr);
        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn any::Any { self }
}
```

### Simulation Bus

```rust
pub struct SimulationBus {
    /// In-flight messages between nodes
    messages: VecDeque<SimMessage>,
    /// Seeded RNG for deterministic scheduling
    rng: StdRng,
    /// Virtual clock
    now: Instant,
    /// Active fault scenarios
    faults: Vec<Fault>,
}

pub enum Fault {
    /// Drop all messages between two nodes
    Partition { a: NodeId, b: NodeId },
    /// Delay messages by a random duration
    Latency { min: Duration, max: Duration },
    /// Duplicate messages with some probability
    Duplication { probability: f64 },
    /// Drop messages with some probability
    MessageLoss { probability: f64 },
    /// Kill a node (stops its reactor)
    NodeCrash { node: NodeId, at: Instant },
    /// Restart a crashed node
    NodeRestart { node: NodeId, at: Instant },
}
```

### Test Harness

```rust
pub struct SimulationHarness {
    seed: u64,
    reactors: Vec<Reactor>,  // one per simulated node
    bus: Rc<RefCell<SimulationBus>>,
}

impl SimulationHarness {
    pub fn run_until(&mut self, condition: impl Fn(&Self) -> bool, max_steps: usize) {
        for _ in 0..max_steps {
            if condition(self) { return; }

            // Advance virtual clock
            self.bus.borrow_mut().advance_time();

            // Deliver/drop/reorder messages based on faults + RNG
            self.bus.borrow_mut().deliver_messages(&mut self.rng);

            // Step each reactor once
            for reactor in &mut self.reactors {
                reactor.step_once();
            }
        }
    }
}

#[test]
fn dst_raft_leader_election_under_partition() {
    for seed in 0..1_000_000 {
        let mut harness = SimulationHarness::new(seed, 5 /* nodes */);
        harness.inject(Fault::Partition { a: 0, b: 1 });
        harness.run_until(|h| h.has_leader(), 10_000);
        harness.assert_at_most_one_leader_per_term();
        harness.assert_committed_entries_consistent();
    }
}
```

---

## Safety Properties (from the Raft paper)

| Property | Description | Checked by |
|---|---|---|
| Election Safety | At most one leader per term | Stateright + DST |
| Leader Append-Only | A leader never overwrites or deletes log entries | Stateright + unit tests |
| Log Matching | Same index + same term = identical prefix | Stateright + DST |
| Leader Completeness | Committed entries appear in all future leaders' logs | Stateright |
| State Machine Safety | No two nodes apply different commands at same index | Stateright + DST |

---

## Project Structure

```
starfish-raft/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── node.rs              # RaftNode pure state machine
│   ├── log.rs               # Log and LogEntry types
│   ├── messages.rs           # RaftInput, RaftOutput enums
│   ├── state_machine.rs      # StateMachine trait
│   ├── config.rs             # Cluster configuration
│   └── integration/
│       ├── mod.rs
│       ├── reactor_adapter.rs   # Wire RaftNode into starfish-reactor
│       └── persistence.rs       # Log persistence to disk
├── tests/
│   ├── unit/
│   │   ├── election_test.rs
│   │   ├── log_replication_test.rs
│   │   └── membership_test.rs
│   ├── stateright/
│   │   ├── actor_adapter.rs     # Stateright Actor impl
│   │   ├── model.rs             # Model configuration
│   │   └── properties.rs        # Safety property definitions
│   └── simulation/
│       ├── simulated_io_manager.rs
│       ├── simulation_bus.rs
│       ├── harness.rs
│       └── scenarios.rs         # Partition, crash, slow-network tests
```

---

## Implementation Order

1. **Pure state machine** — `RaftNode::step()` with election and log replication
2. **Unit tests** — test each RPC handler in isolation
3. **Stateright adapter** — exhaustive model checking of the pure logic
4. **Reactor integration** — wire into starfish-reactor with real IO
5. **SimulatedIOManager** — deterministic simulation testing
6. **Fault scenarios** — partitions, crashes, message loss, slow nodes
7. **Log compaction & snapshots** — optimization pass
8. **Membership changes** — joint consensus (Section 6 of the paper)

---

## References

- [Raft paper (Ongaro & Ousterhout)](https://raft.github.io/raft.pdf)
- [TLA+ spec](https://github.com/ongardie/raft.tla)
- [Stateright](https://github.com/stateright/stateright)
- [FoundationDB simulation testing talk](https://www.youtube.com/watch?v=4fFDFbi3toc)
