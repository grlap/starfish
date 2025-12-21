#![allow(dead_code)]
pub mod data_structures;

// Docs:
// https://raw.githubusercontent.com/kumpera/Lock-free-hastable/refs/heads/master/hash.c
//
// [Jiffy: A Lock-free Skip List with Batch Updates and Snapshots](https://arxiv.org/pdf/2102.01044)
// [LOCK-FREE LINKED LISTS AND SKIP LISTS](https://www.eecs.yorku.ca/~eruppert/Mikhail.pdf)
// [A Provably Correct Scalable Concurrent Skip List](https://people.csail.mit.edu/shanir/publications/OPODIS2006-BA.pdf)
//
// ToDo:
//  - [ ] Cleanup the tests
//  - [ ] Implement Mikhail Mikhailov sorted list
//  - [ ] Implement Epoch memory reclamation
//  - [ ]
// - [ ] Implement
