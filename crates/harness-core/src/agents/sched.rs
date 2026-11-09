//! How many agents run at once. A model request is made to an endpoint: one local server, or one
//! hosted provider. Each endpoint has its own limit (1 for a local server, 3 for a hosted
//! provider by default) and so do all agents together (4). An agent beyond a limit waits for a
//! place; it is not refused.
//!
//! Places are given in the order agents asked, among the agents that can have one: an agent whose
//! endpoint is full does not hold up an agent on another endpoint.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::sync::oneshot;

/// The concurrency limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// At once on one local server.
    pub per_local: usize,
    /// At once on one hosted provider.
    pub per_hosted: usize,
    /// At once in all.
    pub total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            per_local: 1,
            per_hosted: 3,
            total: 4,
        }
    }
}

/// Where an agent's requests go.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// A name for the server or provider; agents with the same key share its limit.
    pub key: String,
    /// Whether it is a server of the user's own.
    pub local: bool,
}

struct Waiter {
    endpoint: Endpoint,
    grant: oneshot::Sender<Permit>,
}

#[derive(Default)]
struct State {
    total: usize,
    per_endpoint: HashMap<String, usize>,
    queue: VecDeque<Waiter>,
}

struct Shared {
    limits: Limits,
    state: Mutex<State>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn fits(&self, state: &State, endpoint: &Endpoint) -> bool {
        let limit = if endpoint.local {
            self.limits.per_local
        } else {
            self.limits.per_hosted
        };
        state.total < self.limits.total
            && state.per_endpoint.get(&endpoint.key).copied().unwrap_or(0) < limit
    }
}

/// Gives out places under the limits.
#[derive(Clone)]
pub struct Scheduler {
    shared: Arc<Shared>,
}

/// A place: held while the agent runs. Dropping it gives the place to the next that can have it.
pub struct Permit {
    shared: Arc<Shared>,
    endpoint: Endpoint,
}

impl Scheduler {
    pub fn new(limits: Limits) -> Self {
        Scheduler {
            shared: Arc::new(Shared {
                limits,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Waits for a place on `endpoint`. Dropping the future gives up the wait, and any place
    /// already granted to it goes to the next.
    pub async fn acquire(&self, endpoint: Endpoint) -> Permit {
        let (grant, granted) = oneshot::channel();
        lock(&self.shared.state)
            .queue
            .push_back(Waiter { endpoint, grant });
        dispatch(&self.shared);
        granted.await.expect("the scheduler outlives its waiters")
    }

    /// How many agents hold a place, and how many wait for one.
    pub fn counts(&self) -> (usize, usize) {
        let state = lock(&self.shared.state);
        (state.total, state.queue.len())
    }
}

/// Gives places to the waiters that can have one, oldest first.
fn dispatch(shared: &Arc<Shared>) {
    let mut granted = Vec::new();
    {
        let mut state = lock(&shared.state);
        let mut i = 0;
        while i < state.queue.len() {
            if state.queue[i].grant.is_closed() {
                state.queue.remove(i);
            } else if shared.fits(&state, &state.queue[i].endpoint) {
                let waiter = state.queue.remove(i).expect("index is in range");
                state.total += 1;
                *state
                    .per_endpoint
                    .entry(waiter.endpoint.key.clone())
                    .or_default() += 1;
                let permit = Permit {
                    shared: shared.clone(),
                    endpoint: waiter.endpoint,
                };
                granted.push((waiter.grant, permit));
                // Earlier waiters were skipped for a full endpoint; this place changed nothing
                // for them, so go on from the same spot.
            } else {
                i += 1;
            }
        }
    }
    // Sent outside the lock: a place whose waiter has gone is dropped here, and giving it back
    // takes the lock.
    for (grant, permit) in granted {
        let _ = grant.send(permit);
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared.state);
            state.total = state.total.saturating_sub(1);
            if let Some(n) = state.per_endpoint.get_mut(&self.endpoint.key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    state.per_endpoint.remove(&self.endpoint.key);
                }
            }
        }
        dispatch(&self.shared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn hosted(key: &str) -> Endpoint {
        Endpoint {
            key: key.into(),
            local: false,
        }
    }

    fn local(key: &str) -> Endpoint {
        Endpoint {
            key: key.into(),
            local: true,
        }
    }

    async fn gets(sched: &Scheduler, endpoint: Endpoint) -> Option<Permit> {
        tokio::time::timeout(Duration::from_millis(50), sched.acquire(endpoint))
            .await
            .ok()
    }

    #[tokio::test]
    async fn a_local_server_runs_one_agent_at_a_time() {
        let sched = Scheduler::new(Limits::default());
        let first = gets(&sched, local("ollama")).await.expect("first");
        assert!(gets(&sched, local("ollama")).await.is_none());
        // Another server is another limit.
        assert!(gets(&sched, local("lmstudio")).await.is_some());
        drop(first);
        assert!(gets(&sched, local("ollama")).await.is_some());
    }

    #[tokio::test]
    async fn a_hosted_provider_runs_three() {
        let sched = Scheduler::new(Limits::default());
        let held: Vec<_> = vec![
            gets(&sched, hosted("openai")).await.unwrap(),
            gets(&sched, hosted("openai")).await.unwrap(),
            gets(&sched, hosted("openai")).await.unwrap(),
        ];
        assert!(gets(&sched, hosted("openai")).await.is_none());
        assert!(gets(&sched, hosted("anthropic")).await.is_some());
        drop(held);
    }

    #[tokio::test]
    async fn four_run_in_all_whatever_the_endpoints() {
        let sched = Scheduler::new(Limits::default());
        let mut held = Vec::new();
        for key in ["a", "b", "c", "d"] {
            held.push(gets(&sched, hosted(key)).await.unwrap());
        }
        assert!(gets(&sched, hosted("e")).await.is_none());
        assert_eq!(sched.counts().0, 4);
        held.pop();
        assert!(gets(&sched, hosted("e")).await.is_some());
    }

    #[tokio::test]
    async fn waiters_get_places_in_the_order_they_asked() {
        let sched = Scheduler::new(Limits::default());
        let first = gets(&sched, local("ollama")).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for n in 0..3 {
            let (sched, tx) = (sched.clone(), tx.clone());
            tokio::spawn(async move {
                let permit = sched.acquire(local("ollama")).await;
                tx.send(n).unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
                drop(permit);
            });
            // Let each reach the queue before the next asks.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(sched.counts().1, 3);
        drop(first);
        let mut order = Vec::new();
        for _ in 0..3 {
            order.push(
                tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn a_full_endpoint_does_not_hold_up_another() {
        let sched = Scheduler::new(Limits::default());
        let _held = gets(&sched, local("ollama")).await.unwrap();
        let waiting = {
            let sched = sched.clone();
            tokio::spawn(async move { sched.acquire(local("ollama")).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Queued behind the local one, yet the hosted request goes at once.
        assert!(gets(&sched, hosted("openai")).await.is_some());
        waiting.abort();
    }

    #[tokio::test]
    async fn a_waiter_that_gives_up_leaves_no_place_held() {
        let sched = Scheduler::new(Limits::default());
        let held = gets(&sched, local("ollama")).await.unwrap();
        let waiting = {
            let sched = sched.clone();
            tokio::spawn(async move { sched.acquire(local("ollama")).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        waiting.abort();
        let _ = waiting.await;
        drop(held);
        assert!(
            gets(&sched, local("ollama")).await.is_some(),
            "the place was lost"
        );
        assert_eq!(sched.counts(), (0, 0));
    }
}
