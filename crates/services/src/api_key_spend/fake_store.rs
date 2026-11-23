//! An in-memory [`SpendTotalStore`] with the same semantics as the Lua
//! scripts in [`super::lua`], plus failure, delay and gate injection.

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

#[derive(Default)]
struct Keys {
    /// The raw string, so tests can plant malformed values.
    total: Option<String>,
    pend: Option<i64>,
    generation: i64,
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

    pub(super) fn set_total(&self, id: Uuid, raw: &str) {
        self.keys.lock().unwrap().entry(id).or_default().total = Some(raw.to_string());
    }

    pub(super) fn total(&self, id: Uuid) -> Option<String> {
        self.keys
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|k| k.total.clone())
    }

    pub(super) fn pend(&self, id: Uuid) -> Option<i64> {
        self.keys.lock().unwrap().get(&id).and_then(|k| k.pend)
    }

    pub(super) fn expire_pend(&self, id: Uuid) {
        if let Some(k) = self.keys.lock().unwrap().get_mut(&id) {
            k.pend = None;
        }
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
        // Like the script, never create a key.
        let Some(k) = all.get_mut(&id) else {
            return Ok(0);
        };
        let mut n = 0;
        if let Some(raw) = &k.total {
            // INCRBY on a non-integer value is an error.
            let v: i64 = raw.parse().map_err(|_| StoreErrorKind::Other)?;
            k.total = Some(
                v.checked_add(cost)
                    .ok_or(StoreErrorKind::Other)?
                    .to_string(),
            );
            n += 1;
        }
        if let Some(p) = k.pend {
            k.pend = Some(p + cost);
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
        let all = self.keys.lock().unwrap();
        Ok(match all.get(&id).and_then(|k| k.total.as_deref()) {
            None => TotalRead::Missing,
            Some(raw) => parse_total(raw).map_or(TotalRead::Malformed, TotalRead::Value),
        })
    }

    async fn begin(&self, id: Uuid) -> Result<String, StoreErrorKind> {
        self.enter(id, Call::Begin).await?;
        if let Some(kind) = *self.begin_fail.lock().unwrap() {
            return Err(kind);
        }
        let mut all = self.keys.lock().unwrap();
        let k = all.entry(id).or_default();
        k.pend.get_or_insert(0);
        Ok(k.generation.to_string())
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
        if k.generation.to_string() != expected_generation {
            return Ok(None);
        }
        let Some(pend) = k.pend else {
            return Ok(None);
        };
        if k.total.is_some() {
            return Ok(None);
        }
        let total = sum + pend;
        k.total = Some(total.to_string());
        k.pend = None;
        k.generation += 1;
        Ok(Some(total))
    }

    async fn invalidate(&self, id: Uuid) -> Result<(), StoreErrorKind> {
        self.enter(id, Call::Invalidate).await?;
        let mut all = self.keys.lock().unwrap();
        let k = all.entry(id).or_default();
        k.total = None;
        k.pend = None;
        k.generation += 1;
        Ok(())
    }
}
