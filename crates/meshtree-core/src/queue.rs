//! Shared-operation FIFO queue (pure state machine).
//!
//! Authoritative specification: `SPEC.md` §7 (`QueueTicket`), §12 (per-queue
//! FIFO with `concurrency = N`; a slot is held until the command exits or the
//! holder dies, detected by pid + boot id), §17 and `ADR-0005` (argument
//! overrides limited to a per-queue allow-list).
//!
//! Tickets are kept in request order. Grants are strictly FIFO, so the
//! granted tickets are always a prefix of that order and a ticket's
//! `position` is simply its index: `position < concurrency` ⇔ granted.
//! Liveness is injected (`is_dead`), keeping this module free of processes.

use crate::task::TaskId;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Prefix of queue ticket ids (`"q-" + ULID`, `SPEC.md` §7).
pub const TICKET_ID_PREFIX: &str = "q-";

/// The process holding (or waiting for) a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub boot_id: String,
}

/// `QueueTicket` (`SPEC.md` §7), plus the holder identity and the accepted
/// argument overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTicket {
    pub id: String,
    pub queue: String,
    pub task: TaskId,
    /// Tickets ahead of this one (granted holders first).
    pub position: usize,
    pub granted: bool,
    pub granted_at: Option<Timestamp>,
    pub holder: Holder,
    /// Extra arguments appended to the queue command (allow-listed).
    #[serde(default)]
    pub args: Vec<String>,
}

/// A queue request that was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueError(pub String);

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueueError {}

/// Characters allowed in the value of a `--flag=*` override.
fn safe_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/:@+-".contains(c))
}

/// Checks `args` against a queue's `allow_args` (`ADR-0005`).
pub fn check_overrides(queue: &str, allowed: &[String], args: &[String]) -> Result<(), QueueError> {
    for arg in args {
        let ok = allowed.iter().any(|entry| match entry.strip_suffix('*') {
            Some(prefix) if prefix.ends_with('=') => {
                arg.strip_prefix(prefix).is_some_and(safe_value)
            }
            _ => entry == arg,
        });
        if !ok {
            return Err(QueueError(format!(
                "argument '{arg}' is not allow-listed for queue '{queue}' (allow_args, SPEC §17)"
            )));
        }
    }
    Ok(())
}

/// One named FIFO queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharedQueue {
    pub name: String,
    pub concurrency: usize,
    tickets: Vec<QueueTicket>,
}

impl SharedQueue {
    /// An empty queue (`concurrency` ≥ 1, validated by config).
    pub fn new(name: impl Into<String>, concurrency: usize) -> Self {
        Self {
            name: name.into(),
            concurrency: concurrency.max(1),
            tickets: Vec::new(),
        }
    }

    /// Tickets in FIFO order.
    pub fn tickets(&self) -> &[QueueTicket] {
        &self.tickets
    }

    /// A ticket by id.
    pub fn get(&self, id: &str) -> Option<&QueueTicket> {
        self.tickets.iter().find(|t| t.id == id)
    }

    /// Appends a new waiting ticket and grants it at once if a slot is free.
    /// Returns the stored ticket.
    pub fn request(
        &mut self,
        id: String,
        task: TaskId,
        holder: Holder,
        args: Vec<String>,
        now: Timestamp,
    ) -> &QueueTicket {
        self.tickets.push(QueueTicket {
            id,
            queue: self.name.clone(),
            task,
            position: self.tickets.len(),
            granted: false,
            granted_at: None,
            holder,
            args,
        });
        self.promote(now);
        self.tickets.last().expect("just pushed")
    }

    /// Removes a ticket (slot released, or a waiter giving up). Returns it.
    pub fn release(&mut self, id: &str, now: Timestamp) -> Option<QueueTicket> {
        let index = self.tickets.iter().position(|t| t.id == id)?;
        let ticket = self.tickets.remove(index);
        self.promote(now);
        Some(ticket)
    }

    /// Removes every ticket whose holder `is_dead`, granted or waiting, so a
    /// killed agent never wedges the queue (§12). Returns the evicted tickets.
    pub fn evict(&mut self, is_dead: impl Fn(&Holder) -> bool, now: Timestamp) -> Vec<QueueTicket> {
        let (dead, live): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tickets)
            .into_iter()
            .partition(|t| is_dead(&t.holder));
        self.tickets = live;
        self.promote(now);
        dead
    }

    /// Grants waiting tickets in FIFO order while slots are free and
    /// refreshes positions.
    fn promote(&mut self, now: Timestamp) {
        for (position, ticket) in self.tickets.iter_mut().enumerate() {
            ticket.position = position;
            if position < self.concurrency && !ticket.granted {
                ticket.granted = true;
                ticket.granted_at = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(pid: u32) -> Holder {
        Holder {
            pid,
            boot_id: "boot-1".into(),
        }
    }

    fn task(n: u128) -> TaskId {
        TaskId::from_ulid(n)
    }

    fn ts(ms: u64) -> Timestamp {
        Timestamp::from_unix_ms(ms)
    }

    fn state(q: &SharedQueue) -> Vec<(&str, usize, bool)> {
        q.tickets()
            .iter()
            .map(|t| (t.id.as_str(), t.position, t.granted))
            .collect()
    }

    #[test]
    fn queue_fifo_grants_in_request_order_up_to_concurrency() {
        let mut q = SharedQueue::new("test", 2);
        for (i, id) in ["q-a", "q-b", "q-c", "q-d"].into_iter().enumerate() {
            let t = q.request(
                id.into(),
                task(i as u128),
                holder(i as u32),
                vec![],
                ts(i as u64),
            );
            assert_eq!(t.position, i);
            assert_eq!(t.granted, i < 2);
        }
        assert_eq!(
            state(&q),
            [
                ("q-a", 0, true),
                ("q-b", 1, true),
                ("q-c", 2, false),
                ("q-d", 3, false)
            ]
        );
        assert_eq!(q.get("q-a").unwrap().granted_at, Some(ts(0)));

        // Releasing a holder grants the oldest waiter, never a later one.
        let gone = q.release("q-b", ts(10)).unwrap();
        assert_eq!(gone.id, "q-b");
        assert_eq!(
            state(&q),
            [("q-a", 0, true), ("q-c", 1, true), ("q-d", 2, false)]
        );
        assert_eq!(q.get("q-c").unwrap().granted_at, Some(ts(10)));
        assert_eq!(q.get("q-a").unwrap().granted_at, Some(ts(0)), "unchanged");

        // A waiter leaving does not grant anyone.
        assert!(q.release("q-d", ts(11)).is_some());
        assert!(q.release("q-d", ts(12)).is_none());
        assert_eq!(state(&q), [("q-a", 0, true), ("q-c", 1, true)]);
    }

    #[test]
    fn queue_evicts_dead_holders_and_waiters() {
        let mut q = SharedQueue::new("build", 1);
        q.request("q-1".into(), task(1), holder(10), vec![], ts(0));
        q.request("q-2".into(), task(2), holder(20), vec![], ts(0));
        q.request("q-3".into(), task(3), holder(30), vec![], ts(0));
        q.request("q-4".into(), task(4), holder(40), vec![], ts(0));
        // Holder 10 (granted) and waiter 30 died.
        let evicted = q.evict(|h| h.pid == 10 || h.pid == 30, ts(5));
        let ids: Vec<&str> = evicted.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["q-1", "q-3"]);
        assert_eq!(state(&q), [("q-2", 0, true), ("q-4", 1, false)]);
        assert!(q.evict(|_| false, ts(6)).is_empty());
    }

    #[test]
    fn queue_overrides_must_be_allow_listed() {
        let allowed = vec!["--verbose".to_string(), "--filter=*".to_string()];
        let ok = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            check_overrides("test", &allowed, &args).is_ok()
        };
        assert!(ok(&[]));
        assert!(ok(&["--verbose"]));
        assert!(ok(&["--filter=src/auth", "--verbose"]));
        assert!(ok(&["--filter=a.b_c-d:e@f+g"]));
        assert!(!ok(&["--verbose2"]));
        assert!(!ok(&["--filter="]), "empty value");
        assert!(!ok(&["--filter=x;rm -rf /"]), "shell syntax in value");
        assert!(!ok(&["--filter=$(id)"]));
        assert!(!ok(&["--filter"]), "bare flag needs its own entry");
        assert!(!ok(&["&&", "echo"]));
        let err = check_overrides("test", &[], &["--verbose".to_string()]).unwrap_err();
        assert!(err.to_string().contains("not allow-listed"), "{err}");
    }

    #[test]
    fn queue_ticket_wire_shape() {
        let mut q = SharedQueue::new("test", 1);
        let t = q
            .request("q-01JAY".into(), task(7), holder(1), vec![], ts(1_000))
            .clone();
        let v = serde_json::to_value(&t).unwrap();
        for key in ["id", "queue", "task", "position", "granted", "granted_at"] {
            assert!(v.get(key).is_some(), "§7 field {key}");
        }
        assert_eq!(v["granted_at"], "1970-01-01T00:00:01.000Z");
        let back: QueueTicket = serde_json::from_value(v).unwrap();
        assert_eq!(back, t);
    }
}
