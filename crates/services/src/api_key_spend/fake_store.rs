//! An in-memory [`SpendTotalStore`] that mirrors the functions of the library
//! `spend_total.lua` step by step, plus failure, delay and gate injection.
//!
//! The library file is the source of truth: when it changes, change this file
//! to match (tokens that must equal the stored one, a total or pending key
//! without an expiry is dropped when read, `add` never creates a key, the
//! errors a malformed value raises). The test against a real server
//! (`valkey_store_tests`) runs the library itself through the real adapter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::ports::{SpendTotalStore, StoreErrorKind, TotalRead};
use super::valkey_store::parse_total;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Call {
    Get,
    Begin,
    Add(i64),
    Finish { sum: i64, generation: String },
    Invalidate,
}

/// A stored string and whether it has an expiry.
#[derive(Clone, Debug)]
struct Val {
    raw: String,
    expiry: bool,
}

impl Val {
    fn with_expiry(raw: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            expiry: true,
        }
    }
}

#[derive(Default)]
struct Keys {
    /// The raw string, so tests can plant malformed values.
    total: Option<Val>,
    pend: Option<Val>,
    generation: Option<Val>,
}

fn token() -> String {
    Uuid::new_v4().to_string()
}

/// The library's `live`: the value, or nil, deleting a key that has no expiry.
fn live(slot: &mut Option<Val>) -> Option<String> {
    match slot {
        Some(v) if v.expiry => Some(v.raw.clone()),
        Some(_) => {
            *slot = None;
            None
        }
        None => None,
    }
}

/// `INCRBY` on a stored string: the value must be a canonical `i64` and the
/// result must fit, else the command (and the function) fails.
fn incrby(slot: &mut Option<Val>, by: i64) -> Result<i64, StoreErrorKind> {
    let v = slot.as_mut().ok_or(StoreErrorKind::Other)?;
    let n: i64 = v.raw.parse().map_err(|_| StoreErrorKind::Other)?;
    if n.to_string() != v.raw {
        return Err(StoreErrorKind::Other);
    }
    let n = n.checked_add(by).ok_or(StoreErrorKind::Other)?;
    v.raw = n.to_string();
    Ok(n)
}

/// The library's `is_int`.
fn is_int(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Default)]
pub(super) struct FakeStore {
    keys: Mutex<HashMap<Uuid, Keys>>,
    calls: Mutex<Vec<(Uuid, Call)>>,
    fail: Mutex<Option<StoreErrorKind>>,
    /// Fails only BEGIN.
    begin_fail: Mutex<Option<StoreErrorKind>>,
    delay: Mutex<Duration>,
    /// When set, `add` waits for a permit before it runs.
    add_gate: Mutex<Option<Arc<Semaphore>>>,
}

impl FakeStore {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn fail_with(&self, kind: Option<StoreErrorKind>) {
        *self.fail.lock().unwrap() = kind;
    }

    pub(super) fn fail_begin_with(&self, kind: Option<StoreErrorKind>) {
        *self.begin_fail.lock().unwrap() = kind;
    }

    pub(super) fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    pub(super) fn gate_adds(&self, gate: Arc<Semaphore>) {
        *self.add_gate.lock().unwrap() = Some(gate);
    }

    /// Plants a total with an expiry.
    pub(super) fn set_total(&self, id: Uuid, raw: &str) {
        self.keys.lock().unwrap().entry(id).or_default().total = Some(Val::with_expiry(raw));
    }

    /// Plants a total without an expiry (not written by the library).
    pub(super) fn set_total_without_expiry(&self, id: Uuid, raw: &str) {
        self.keys.lock().unwrap().entry(id).or_default().total = Some(Val {
            raw: raw.to_string(),
            expiry: false,
        });
    }

    /// Plants pending costs without an expiry (not written by the library).
    pub(super) fn set_pend_without_expiry(&self, id: Uuid, raw: &str) {
        self.keys.lock().unwrap().entry(id).or_default().pend = Some(Val {
            raw: raw.to_string(),
            expiry: false,
        });
    }

    /// The stored total, if any (inspection: nothing is dropped).
    pub(super) fn total(&self, id: Uuid) -> Option<String> {
        self.keys
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|k| k.total.as_ref().map(|v| v.raw.clone()))
    }

    pub(super) fn pend(&self, id: Uuid) -> Option<i64> {
        self.keys
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|k| k.pend.as_ref().and_then(|v| v.raw.parse().ok()))
    }

    pub(super) fn generation(&self, id: Uuid) -> Option<String> {
        self.keys
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|k| k.generation.as_ref().map(|v| v.raw.clone()))
    }

    pub(super) fn expire_pend(&self, id: Uuid) {
        if let Some(k) = self.keys.lock().unwrap().get_mut(&id) {
            k.pend = None;
        }
    }

    /// The store loses all its data (a restart without persistence, a
    /// failover to an empty replica).
    pub(super) fn wipe(&self) {
        self.keys.lock().unwrap().clear();
    }

    pub(super) fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, c)| c.clone())
            .collect()
    }

    pub(super) fn calls_for(&self, id: Uuid) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(i, _)| *i == id)
            .map(|(_, c)| c.clone())
            .collect()
    }

    pub(super) fn count(&self, wanted: impl Fn(&Call) -> bool) -> usize {
        self.calls().iter().filter(|c| wanted(c)).count()
    }

    /// ADD, synchronously (a usage row committing at a chosen moment).
    pub(super) fn add_now(&self, id: Uuid, cost: i64) -> Result<u8, StoreErrorKind> {
        self.calls.lock().unwrap().push((id, Call::Add(cost)));
        self.add_inner(id, cost)
    }

    fn add_inner(&self, id: Uuid, cost: i64) -> Result<u8, StoreErrorKind> {
        let mut all = self.keys.lock().unwrap();
        // Like the function, never create a key.
        let Some(k) = all.get_mut(&id) else {
            return Ok(0);
        };
        let mut n = 0;
        if live(&mut k.total).is_some() {
            incrby(&mut k.total, cost)?;
            n += 1;
        }
        if live(&mut k.pend).is_some() {
            incrby(&mut k.pend, cost)?;
            n += 2;
        }
        Ok(n)
    }

    async fn enter(&self, id: Uuid, call: Call) -> Result<(), StoreErrorKind> {
        self.calls.lock().unwrap().push((id, call));
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match *self.fail.lock().unwrap() {
            Some(kind) => Err(kind),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl SpendTotalStore for FakeStore {
    async fn get_total(&self, id: Uuid) -> Result<TotalRead, StoreErrorKind> {
        self.enter(id, Call::Get).await?;
        let mut all = self.keys.lock().unwrap();
        let Some(k) = all.get_mut(&id) else {
            return Ok(TotalRead::Missing);
        };
        Ok(match live(&mut k.total) {
            None => TotalRead::Missing,
            Some(raw) => parse_total(&raw).map_or(TotalRead::Malformed, TotalRead::Value),
        })
    }

    async fn begin(&self, id: Uuid) -> Result<String, StoreErrorKind> {
        self.enter(id, Call::Begin).await?;
        if let Some(kind) = *self.begin_fail.lock().unwrap() {
            return Err(kind);
        }
        let mut all = self.keys.lock().unwrap();
        let k = all.entry(id).or_default();
        let g = k.generation.as_ref().map_or_else(token, |v| v.raw.clone());
        k.generation = Some(Val::with_expiry(g.clone()));
        let p = live(&mut k.pend).unwrap_or_else(|| "0".to_string());
        k.pend = Some(Val::with_expiry(p));
        Ok(g)
    }

    async fn add(&self, id: Uuid, cost: i64) -> Result<u8, StoreErrorKind> {
        self.calls.lock().unwrap().push((id, Call::Add(cost)));
        let gate = self.add_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        if let Some(kind) = *self.fail.lock().unwrap() {
            return Err(kind);
        }
        self.add_inner(id, cost)
    }

    async fn finish(
        &self,
        id: Uuid,
        sum: i64,
        expected_generation: &str,
    ) -> Result<Option<i64>, StoreErrorKind> {
        self.enter(
            id,
            Call::Finish {
                sum,
                generation: expected_generation.to_string(),
            },
        )
        .await?;
        let mut all = self.keys.lock().unwrap();
        let k = all.entry(id).or_default();
        // A missing token never equals the expected one.
        if k.generation.as_ref().map(|v| v.raw.as_str()) != Some(expected_generation) {
            return Ok(None);
        }
        let Some(pend) = live(&mut k.pend) else {
            return Ok(None);
        };
        if !is_int(&pend) {
            return Err(StoreErrorKind::Other);
        }
        if live(&mut k.total).is_some() {
            return Ok(None);
        }
        // SET, then INCRBY: an overflow fails after the SET, as in the
        // function.
        k.total = Some(Val::with_expiry(sum.to_string()));
        let pend: i64 = pend.parse().map_err(|_| StoreErrorKind::Other)?;
        let total = incrby(&mut k.total, pend)?;
        k.pend = None;
        k.generation = Some(Val::with_expiry(token()));
        Ok(Some(total))
    }

    async fn invalidate(&self, id: Uuid) -> Result<(), StoreErrorKind> {
        self.enter(id, Call::Invalidate).await?;
        let mut all = self.keys.lock().unwrap();
        let k = all.entry(id).or_default();
        k.total = None;
        k.pend = None;
        k.generation = Some(Val::with_expiry(token()));
        Ok(())
    }
}
