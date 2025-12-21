/*
Epoch:
 - [x] Move Epoch to it's own crate
 - [x] Integrate Epoch with the Coordinator and Reactor
 - [ ] Drop EpochCounters on Coordinator join_all
 - [ ] Implement Acquire EpochToken
 - [ ] Implement Release EpochToken
 - [ ] Keep list of Resources that implement Drop trait
 - [ ] Design resource model.
  - [ ] Resource is assigned to Epoch (using EpochToken)
  - [ ] Do I need resources for a given Epoch ?
    - [ ] case scenario e.g. get MemTable, how would this work
      - create a MemTable using current EpochToken
      - register decommission of MemTable in Epoch
      - get current MemTable <-
      - when using MemTable acquire EpochToken
      - when done using drop EpochToken
     - [ ] get active MemTable, increase counter
     - [ ] when done with MemTable, decrease counter
*/

pub mod epoch;
pub mod epoch_alloc;
