use super::*;
use codex_protocol::error::CodexErrorDetails;
use std::time::Duration;

const NATIVE_AGENT_FORCE_SETTLE_GRACE: Duration = Duration::from_millis(250);

impl AgentControl {
    /// Settle one native-owned worker. Graceful shutdown preserves its rollout. If that bounded
    /// path stalls, abort the turn and session loop before releasing shared ownership records.
    pub(crate) async fn settle_native_agent(
        &self,
        agent_id: ThreadId,
        graceful_timeout: Duration,
    ) -> CodexResult<()> {
        // Native workers are terminal run-owned resources. Close their durable edge before any
        // graceful or forced path can release the in-memory thread/registry ownership. Missing
        // edges are a successful no-op for transactional failures before edge publication.
        let edge_result = self.close_native_spawn_edge(agent_id).await;
        let graceful = async {
            #[cfg(test)]
            {
                let delay = self
                    .native_test_hooks
                    .graceful_shutdown_delay_ms
                    .swap(0, std::sync::atomic::Ordering::AcqRel);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
            self.shutdown_agent_tree(agent_id).await
        };
        let settlement_result = match tokio::time::timeout(graceful_timeout, graceful).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error))
                if matches!(
                    error.details(),
                    CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                ) =>
            {
                self.force_settle_native_agent_tree(agent_id).await
            }
            Ok(Err(error)) => {
                self.force_settle_native_agent_tree(agent_id).await?;
                Err(error)
            }
            Err(_) => self.force_settle_native_agent_tree(agent_id).await,
        };
        match (edge_result, settlement_result) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(edge_error), Err(settlement_error)) => Err(CodexErr::Fatal(format!(
                "failed to close native spawn edge: {edge_error}; native cleanup also failed: {settlement_error}"
            ))),
        }
    }

    async fn close_native_spawn_edge(&self, agent_id: ThreadId) -> CodexResult<()> {
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(());
        };
        agent_graph_store
            .set_thread_spawn_edge_status(
                agent_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
            )
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to close native thread-spawn edge for {agent_id}: {error}"
                ))
            })
    }

    pub(crate) async fn force_settle_native_agent_tree(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<()> {
        #[cfg(test)]
        {
            let delay = self
                .native_test_hooks
                .force_shutdown_delay_ms
                .swap(0, std::sync::atomic::Ordering::AcqRel);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        let state = self.upgrade()?;
        let mut thread_ids = self
            .live_thread_spawn_descendants(agent_id)
            .await
            .unwrap_or_default();
        thread_ids.push(agent_id);
        for thread_id in thread_ids {
            if let Ok(thread) = state.get_thread(thread_id).await {
                thread
                    .session
                    .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                    .await;
                thread.session.close_unified_exec_processes().await;
                let termination = thread.io.session_loop_termination.clone();
                let _ = thread.io.submit(Op::Shutdown {}).await;
                if tokio::time::timeout(NATIVE_AGENT_FORCE_SETTLE_GRACE, termination)
                    .await
                    .is_err()
                {
                    thread.io.force_abort_and_wait().await;
                }
            }
            let _ = state.remove_thread(&thread_id).await;
            self.forget_v2_residency(thread_id);
            self.state.release_spawned_thread(thread_id);
        }
        Ok(())
    }

    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread.session.ensure_rollout_materialized().await;
            thread.session.flush_rollout().await?;
            let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state
                    .send_op(agent_id, Op::Shutdown {}, /*parent_turn_id*/ None)
                    .await
            };
            thread.wait_until_terminated().await;
            result
        } else {
            state
                .send_op(agent_id, Op::Shutdown {}, /*parent_turn_id*/ None)
                .await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.forget_v2_residency(agent_id);
        self.state.release_spawned_thread(agent_id);
        result
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let known_agent = self.state.agent_metadata_for_thread(agent_id).is_some();
        match state.get_thread(agent_id).await {
            Ok(thread) => {
                if !thread.config_snapshot().await.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
            }
            Err(err)
                if known_agent && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
            {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
            }
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {}
            Err(err) => {
                warn!("failed to inspect agent before close {agent_id}: {err}");
            }
        }
        match Box::pin(self.shutdown_agent_tree(agent_id)).await {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(String::new())
            }
            result => result,
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }
}
