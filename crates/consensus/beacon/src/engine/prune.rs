//! Prune management for the engine implementation.

use futures::{FutureExt, Stream};
use reth_primitives::BlockNumber;
use reth_provider::CanonStateNotification;
use reth_prune::{Pruner, PrunerError, PrunerWithResult};
use reth_tasks::TaskSpawner;
use std::task::{ready, Context, Poll};
use tokio::sync::oneshot;
use tracing::trace;

/// Manages pruning under the control of the engine.
///
/// This type controls the [Pruner].
pub(crate) struct EnginePruneController {
    /// The current state of the pruner.
    pruner_state: PrunerState,
    /// The type that can spawn the pruner task.
    pruner_task_spawner: Box<dyn TaskSpawner>,
}

impl EnginePruneController {
    /// Create a new instance
    pub(crate) fn new(pruner: Pruner, pruner_task_spawner: Box<dyn TaskSpawner>) -> Self {
        Self { pruner_state: PrunerState::Idle(Some(pruner)), pruner_task_spawner }
    }

    /// Returns `true` if the pruner is idle.
    pub(crate) fn is_pruner_idle(&self) -> bool {
        self.pruner_state.is_idle()
    }

    /// Advances the pruner state.
    ///
    /// This checks for the result in the channel, or returns pending if the pruner is idle.
    fn poll_pruner(&mut self, cx: &mut Context<'_>) -> Poll<EnginePruneEvent> {
        let res = match self.pruner_state {
            PrunerState::Idle(_) => return Poll::Pending,
            PrunerState::Running(ref mut fut) => {
                ready!(fut.poll_unpin(cx))
            }
        };
        let ev = match res {
            Ok((pruner, result)) => {
                self.pruner_state = PrunerState::Idle(Some(pruner));
                EnginePruneEvent::Finished { result }
            }
            Err(_) => {
                // failed to receive the pruner
                EnginePruneEvent::TaskDropped
            }
        };
        Poll::Ready(ev)
    }

    /// This will try to spawn the pruner if it is idle:
    /// 1. Try to acquire the tip block number through [Pruner::check_tip].
    /// 2. If tip block number is ready, pass it to the [Pruner::run_as_fut] and spawn in a separate
    /// task. Set pruner state to [PrunerState::Running].
    /// 3. If tip block number is not ready yet, set pruner state back to [PrunerState::Idle].
    ///
    /// If pruner is already running, do nothing.
    fn try_spawn_pruner(&mut self, cx: &mut Context<'_>) -> Option<EnginePruneEvent> {
        match &mut self.pruner_state {
            PrunerState::Idle(pruner) => {
                let mut pruner = pruner.take()?;

                // Check tip for pruning
                match pruner.check_tip(cx) {
                    // If tip is ready, start pruning
                    Some(tip_block_number) => {
                        trace!(target: "consensus::engine::prune", %tip_block_number, "Tip block number for pruning is acquired");

                        let (tx, rx) = oneshot::channel();
                        self.pruner_task_spawner.spawn_critical_blocking(
                            "pruner task",
                            Box::pin(async move {
                                let result = pruner.run_as_fut(tip_block_number).await;
                                let _ = tx.send(result);
                            }),
                        );
                        self.pruner_state = PrunerState::Running(rx);

                        Some(EnginePruneEvent::Started(tip_block_number))
                    }
                    // If tip is not ready yet, make pruner idle again
                    None => {
                        self.pruner_state = PrunerState::Idle(Some(pruner));
                        Some(EnginePruneEvent::NotReady)
                    }
                }
            }
            PrunerState::Running(_) => None,
        }
    }

    /// Advances the prune process.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<EnginePruneEvent> {
        // Try to spawn a pruner
        if let Some(event) = self.try_spawn_pruner(cx) {
            return Poll::Ready(event)
        }

        loop {
            if let Poll::Ready(event) = self.poll_pruner(cx) {
                return Poll::Ready(event)
            }

            if !self.pruner_state.is_idle() {
                // Can not make any progress
                return Poll::Pending
            }
        }
    }
}

/// The event type emitted by the [EnginePruneController].
#[derive(Debug)]
pub(crate) enum EnginePruneEvent {
    /// Pruner is not ready
    NotReady,
    /// Pruner started with tip block number
    Started(BlockNumber),
    /// Pruner finished
    ///
    /// If this is returned, the pruner is idle.
    Finished {
        /// Final result of the pruner run.
        result: Result<(), PrunerError>,
    },
    /// Pruner task was dropped after it was started, unable to receive it because channel
    /// closed. This would indicate a panicked pruner task
    TaskDropped,
}

/// The possible pruner states within the sync controller.
///
/// [PrunerState::Idle] means that the pruner is currently idle.
/// [PrunerState::Running] means that the pruner is currently running.
///
/// NOTE: The differentiation between these two states is important, because when the pruner is
/// running, it acquires the write lock over the database. This means that we cannot forward to the
/// blockchain tree any messages that would result in database writes, since it would result in a
/// deadlock.
enum PrunerState {
    /// Pruner is idle.
    Idle(Option<Pruner>),
    /// Pruner is running and waiting for a response
    Running(oneshot::Receiver<PrunerWithResult>),
}

impl PrunerState {
    /// Returns `true` if the state matches idle.
    fn is_idle(&self) -> bool {
        matches!(self, PrunerState::Idle(_))
    }
}
