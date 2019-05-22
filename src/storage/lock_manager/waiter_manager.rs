// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use super::deadlock::Scheduler as DetectorScheduler;
use super::Lock;
use crate::storage::mvcc::Error as MvccError;
use crate::storage::txn::Error as TxnError;
use crate::storage::txn::{execute_callback, ProcessResult};
use crate::storage::{Error as StorageError, StorageCb};
use crate::tikv_util::collections::HashMap;
use crate::tikv_util::worker::{FutureRunnable, FutureScheduler, Stopped};
use futures::Future;
use kvproto::deadlock::WaitForEntry;
use std::cell::RefCell;
use std::fmt::{self, Debug, Display, Formatter};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio_core::reactor::Handle;
use tokio_timer::Delay;

pub type Callback = Box<dyn FnOnce(Vec<WaitForEntry>) + Send>;

pub enum Task {
    WaitFor {
        // which txn waiting for the lock
        start_ts: u64,
        cb: StorageCb,
        pr: ProcessResult,
        lock: Lock,
        is_first_lock: bool,
    },
    WakeUp {
        // lock info
        lock_ts: u64,
        hashes: Vec<u64>,
        commit_ts: u64,
    },
    Dump {
        cb: Callback,
    },
    Deadlock {
        start_ts: u64,
        lock: Lock,
        deadlock_key_hash: u64,
    },
}

/// Debug for task.
impl Debug for Task {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

/// Display for task.
impl Display for Task {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Task::WaitFor { start_ts, lock, .. } => {
                write!(f, "txn:{} waiting for {}:{}", start_ts, lock.ts, lock.hash)
            }
            Task::WakeUp { lock_ts, .. } => write!(f, "waking up txns waiting for {}", lock_ts),
            Task::Dump { .. } => write!(f, "dump"),
            Task::Deadlock { start_ts, .. } => write!(f, "txn:{} deadlock", start_ts),
        }
    }
}

struct Waiter {
    start_ts: u64,
    cb: StorageCb,
    pr: ProcessResult,
    lock: Lock,
}

type Waiters = Vec<Waiter>;

struct WaitTable {
    wait_table: HashMap<u64, Waiters>,
}

impl WaitTable {
    fn new() -> Self {
        Self {
            wait_table: HashMap::new(),
        }
    }

    #[allow(unused)]
    fn size(&self) -> usize {
        self.wait_table.iter().map(|(_, v)| v.len()).sum()
    }

    fn add_waiter(&mut self, ts: u64, waiter: Waiter) -> bool {
        self.wait_table.entry(ts).or_insert(vec![]).push(waiter);
        true
    }

    fn get_ready_waiters(&mut self, ts: u64, mut hashes: Vec<u64>) -> Waiters {
        hashes.sort_unstable();
        let mut ready_waiters = vec![];
        if let Some(waiters) = self.wait_table.get_mut(&ts) {
            let mut i = 0;
            let mut count = waiters.len();
            while count > 0 {
                if hashes.binary_search(&waiters[i].lock.hash).is_ok() {
                    ready_waiters.push(waiters.swap_remove(i));
                } else {
                    i += 1;
                }
                count -= 1;
            }
            if waiters.is_empty() {
                self.wait_table.remove(&ts);
            }
        }
        ready_waiters
    }

    fn remove_waiter(&mut self, start_ts: u64, lock: Lock) -> Option<Waiter> {
        if let Some(waiters) = self.wait_table.get_mut(&lock.ts) {
            let idx = waiters
                .iter()
                .position(|waiter| waiter.start_ts == start_ts && waiter.lock.hash == lock.hash);
            if let Some(idx) = idx {
                let waiter = waiters.remove(idx);
                if waiters.is_empty() {
                    self.wait_table.remove(&lock.ts);
                }
                return Some(waiter);
            }
        }
        None
    }

    fn to_wait_for_entries(&self) -> Vec<WaitForEntry> {
        self.wait_table
            .iter()
            .flat_map(|(_, waiters)| {
                waiters.iter().map(|waiter| {
                    let mut wait_for_entry = WaitForEntry::new();
                    wait_for_entry.set_txn(waiter.start_ts);
                    wait_for_entry.set_wait_for_txn(waiter.lock.ts);
                    wait_for_entry.set_key_hash(waiter.lock.hash);
                    wait_for_entry
                })
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct Scheduler(FutureScheduler<Task>);

impl Scheduler {
    pub fn new(scheduler: FutureScheduler<Task>) -> Self {
        Self(scheduler)
    }

    fn notify_scheduler(&self, task: Task) -> bool {
        if let Err(Stopped(task)) = self.0.schedule(task) {
            error!("failed to send task to waiter_manager"; "task" => %task);
            if let Task::WaitFor { cb, pr, .. } = task {
                execute_callback(cb, pr);
            }
            return false;
        }
        true
    }

    pub fn wait_for(
        &self,
        start_ts: u64,
        cb: StorageCb,
        pr: ProcessResult,
        lock: Lock,
        is_first_lock: bool,
    ) {
        self.notify_scheduler(Task::WaitFor {
            start_ts,
            cb,
            pr,
            lock,
            is_first_lock,
        });
    }

    pub fn wake_up(&self, lock_ts: u64, hashes: Vec<u64>, commit_ts: u64) {
        self.notify_scheduler(Task::WakeUp {
            lock_ts,
            hashes,
            commit_ts,
        });
    }

    pub fn dump_wait_table(&self, cb: Callback) -> bool {
        self.notify_scheduler(Task::Dump { cb })
    }

    pub fn deadlock(&self, txn_ts: u64, lock: Lock, deadlock_key_hash: u64) {
        self.notify_scheduler(Task::Deadlock {
            start_ts: txn_ts,
            lock,
            deadlock_key_hash,
        });
    }
}

/// WaiterManager handles waiting and wake-up of pessimistic lock
pub struct WaiterManager {
    wait_table: Rc<RefCell<WaitTable>>,
    detector_scheduler: DetectorScheduler,
    wait_for_lock_timeout: u64,
    wake_up_delay_duration: u64,
}

unsafe impl Send for WaiterManager {}

impl WaiterManager {
    pub fn new(
        detector_scheduler: DetectorScheduler,
        wait_for_lock_timeout: u64,
        wake_up_delay_duration: u64,
    ) -> Self {
        Self {
            wait_table: Rc::new(RefCell::new(WaitTable::new())),
            detector_scheduler,
            wait_for_lock_timeout,
            wake_up_delay_duration,
        }
    }

    fn handle_wait_for(&mut self, handle: &Handle, is_first_lock: bool, waiter: Waiter) {
        let lock = waiter.lock.clone();
        let start_ts = waiter.start_ts;

        // If it is the first lock, deadlock never occur
        if !is_first_lock {
            self.detector_scheduler.detect(start_ts, lock.clone());
        }
        if self.wait_table.borrow_mut().add_waiter(lock.ts, waiter) {
            let wait_table = Rc::clone(&self.wait_table);
            let detector_scheduler = self.detector_scheduler.clone();
            let when = Instant::now() + Duration::from_millis(self.wait_for_lock_timeout);
            // TODO: cancel timer when wake up.
            let timer = Delay::new(when)
                .map_err(|e| info!("timeout timer delay errored"; "err" => ?e))
                .then(move |_| {
                    wait_table
                        .borrow_mut()
                        .remove_waiter(start_ts, lock.clone())
                        .and_then(|waiter| {
                            detector_scheduler.clean_up_wait_for(start_ts, lock);
                            execute_callback(waiter.cb, waiter.pr);
                            Some(())
                        });
                    Ok(())
                });
            handle.spawn(timer);
        }
    }

    fn handle_wake_up(&mut self, handle: &Handle, lock_ts: u64, hashes: Vec<u64>, commit_ts: u64) {
        let mut ready_waiters = self
            .wait_table
            .borrow_mut()
            .get_ready_waiters(lock_ts, hashes);
        ready_waiters.sort_unstable_by_key(|waiter| waiter.start_ts);
        for (i, waiter) in ready_waiters.into_iter().enumerate() {
            self.detector_scheduler
                .clean_up_wait_for(waiter.start_ts, waiter.lock.clone());
            if self.wake_up_delay_duration > 0 {
                // Sleep a little so the transaction with small start_ts will more likely get the lock.
                let when = Instant::now()
                    + Duration::from_millis(self.wake_up_delay_duration * (i as u64));
                let timer = Delay::new(when)
                    .and_then(move |_| {
                        wake_up_waiter(waiter, commit_ts);
                        Ok(())
                    })
                    .map_err(|e| info!("wake-up timer delay errored"; "err" => ?e));
                handle.spawn(timer);
            } else {
                wake_up_waiter(waiter, commit_ts);
            }
        }
    }

    fn handle_dump(&self, cb: Callback) {
        cb(self.wait_table.borrow().to_wait_for_entries());
    }

    fn handle_deadlock(&mut self, start_ts: u64, lock: Lock, deadlock_key_hash: u64) {
        self.wait_table
            .borrow_mut()
            .remove_waiter(start_ts, lock)
            .and_then(|waiter| {
                let pr = ProcessResult::Failed {
                    err: StorageError::from(MvccError::Deadlock {
                        start_ts,
                        lock_ts: waiter.lock.ts,
                        key_hash: waiter.lock.hash,
                        deadlock_key_hash,
                    }),
                };
                execute_callback(waiter.cb, pr);
                Some(())
            });
    }
}

impl FutureRunnable<Task> for WaiterManager {
    fn run(&mut self, task: Task, handle: &Handle) {
        match task {
            Task::WaitFor {
                start_ts,
                cb,
                pr,
                lock,
                is_first_lock,
            } => {
                self.handle_wait_for(
                    handle,
                    is_first_lock,
                    Waiter {
                        start_ts,
                        cb,
                        pr,
                        lock,
                    },
                );
            }
            Task::WakeUp {
                lock_ts,
                hashes,
                commit_ts,
            } => {
                self.handle_wake_up(handle, lock_ts, hashes, commit_ts);
            }
            Task::Dump { cb } => {
                self.handle_dump(cb);
            }
            Task::Deadlock {
                start_ts,
                lock,
                deadlock_key_hash,
            } => {
                self.handle_deadlock(start_ts, lock, deadlock_key_hash);
            }
        }
    }
}

fn wake_up_waiter(waiter: Waiter, commit_ts: u64) {
    // Maybe we can store the latest commit_ts in TiKV, and use
    // it as `conflict_start_ts` when waker's `conflict_commit_ts`
    // is smaller than waiter's for_update_ts.
    //
    // If so TiDB can use this `conflict_start_ts` as `for_update_ts`
    // directly, there is no need to get a ts from PD.
    let mvcc_err = MvccError::WriteConflict {
        start_ts: waiter.start_ts,
        conflict_start_ts: waiter.lock.ts,
        conflict_commit_ts: commit_ts,
        key: vec![],
        primary: vec![],
    };
    let pr = ProcessResult::Failed {
        err: StorageError::from(TxnError::from(mvcc_err)),
    };
    execute_callback(waiter.cb, pr);
}

#[cfg(test)]
mod tests {
    use super::super::util::*;
    use super::*;
    use crate::storage::Key;
    use std::time::Duration;
    use test_util::KvGenerator;

    fn dummy_waiter(start_ts: u64, lock_ts: u64, hash: u64) -> Waiter {
        Waiter {
            start_ts,
            cb: StorageCb::Boolean(Box::new(|_| ())),
            pr: ProcessResult::Res,
            lock: Lock { ts: lock_ts, hash },
        }
    }

    #[test]
    fn test_wait_table_add_and_remove() {
        let mut wait_table = WaitTable::new();
        for i in 0..10 {
            let n = i as u64;
            wait_table.add_waiter(n, dummy_waiter(0, n, n));
        }
        assert_eq!(10, wait_table.size());
        for i in (0..10).rev() {
            let n = i as u64;
            assert!(wait_table
                .remove_waiter(0, Lock { ts: n, hash: n })
                .is_some());
        }
        assert_eq!(0, wait_table.size());
        assert!(wait_table
            .remove_waiter(0, Lock { ts: 0, hash: 0 })
            .is_none());
    }

    #[test]
    fn test_wait_table_get_ready_waiters() {
        let mut wait_table = WaitTable::new();
        let ts = 100;
        let mut hashes: Vec<u64> = KvGenerator::new(64, 0)
            .generate(10)
            .into_iter()
            .map(|(key, _)| gen_key_hash(&Key::from_raw(&key)))
            .collect();

        assert!(wait_table.get_ready_waiters(ts, hashes.clone()).is_empty());

        for hash in hashes.iter() {
            wait_table.add_waiter(ts, dummy_waiter(0, ts, *hash));
        }
        hashes.sort();

        let not_ready = hashes.split_off(hashes.len() / 2);
        let ready_waiters = wait_table.get_ready_waiters(ts, hashes.clone());
        assert_eq!(hashes.len(), ready_waiters.len());
        assert_eq!(not_ready.len(), wait_table.size());

        let ready_waiters = wait_table.get_ready_waiters(ts, hashes.clone());
        assert!(ready_waiters.is_empty());

        let ready_waiters = wait_table.get_ready_waiters(ts, not_ready.clone());
        assert_eq!(not_ready.len(), ready_waiters.len());
        assert_eq!(0, wait_table.size());
    }

    #[test]
    fn test_wait_table_to_wait_for_entries() {
        let mut wait_table = WaitTable::new();
        assert!(wait_table.to_wait_for_entries().is_empty());

        for i in 1..5 {
            for j in 0..i {
                wait_table.add_waiter(i, dummy_waiter(i * 10 + j, i, j));
            }
        }

        let mut wait_for_enties = wait_table.to_wait_for_entries();
        wait_for_enties.sort_by_key(|e| e.txn);
        wait_for_enties.reverse();
        for i in 1..5 {
            for j in 0..i {
                let e = wait_for_enties.pop().unwrap();
                assert_eq!(e.get_txn(), i * 10 + j);
                assert_eq!(e.get_wait_for_txn(), i);
                assert_eq!(e.get_key_hash(), j);
            }
        }
        assert!(wait_for_enties.is_empty());
    }

    #[test]
    fn test_waiter_manager() {
        use crate::tikv_util::worker::FutureWorker;
        use std::sync::mpsc;

        let detect_worker = FutureWorker::new("dummy-deadlock");
        let detector_scheduler = DetectorScheduler::new(detect_worker.scheduler());

        let mut waiter_mgr_worker = FutureWorker::new("lock-manager");
        let waiter_mgr_runner = WaiterManager::new(detector_scheduler, 1000, 1);
        let waiter_mgr_scheduler = Scheduler::new(waiter_mgr_worker.scheduler());
        waiter_mgr_worker.start(waiter_mgr_runner).unwrap();

        // timeout
        let (tx, rx) = mpsc::channel();
        let cb = Box::new(move |result| {
            tx.send(result).unwrap();
        });
        let pr = ProcessResult::Res;
        waiter_mgr_scheduler.wait_for(0, StorageCb::Boolean(cb), pr, Lock { ts: 0, hash: 0 }, true);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(1100))
                .unwrap()
                .unwrap(),
            ()
        );

        // wake-up
        let (tx, rx) = mpsc::channel();
        let cb = Box::new(move |result| {
            tx.send(result).unwrap();
        });
        waiter_mgr_scheduler.wait_for(
            0,
            StorageCb::Boolean(cb),
            ProcessResult::Res,
            Lock { ts: 0, hash: 1 },
            true,
        );
        waiter_mgr_scheduler.wake_up(0, vec![3, 1, 2], 1);
        assert!(rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_err());
    }
}
