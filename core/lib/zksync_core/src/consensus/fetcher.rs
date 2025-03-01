use anyhow::Context as _;
use zksync_concurrency::{ctx, error::Wrap as _, scope, time};
use zksync_consensus_executor as executor;
use zksync_consensus_roles::validator;
use zksync_types::MiniblockNumber;
use zksync_web3_decl::client::BoxedL2Client;

use crate::{
    consensus::{storage, Store},
    sync_layer::{
        fetcher::FetchedBlock, sync_action::ActionQueueSender, MainNodeClient, SyncState,
    },
};

pub type P2PConfig = executor::Config;

/// Miniblock fetcher.
pub struct Fetcher {
    pub store: Store,
    pub sync_state: SyncState,
    pub client: BoxedL2Client,
}

impl Fetcher {
    /// Task fetching L2 blocks using peer-to-peer gossip network.
    /// NOTE: it still uses main node json RPC in some cases for now.
    pub async fn run_p2p(
        self,
        ctx: &ctx::Ctx,
        actions: ActionQueueSender,
        p2p: P2PConfig,
    ) -> anyhow::Result<()> {
        let res: ctx::Result<()> = scope::run!(ctx, |ctx, s| async {
            // Update sync state in the background.
            s.spawn_bg(self.fetch_state_loop(ctx));

            // Initialize genesis.
            let genesis = self.fetch_genesis(ctx).await.wrap("fetch_genesis()")?;
            let mut conn = self.store.access(ctx).await.wrap("access()")?;
            conn.try_update_genesis(ctx, &genesis)
                .await
                .wrap("set_genesis()")?;
            let mut payload_queue = conn
                .new_payload_queue(ctx, actions)
                .await
                .wrap("new_payload_queue()")?;
            drop(conn);

            // Fetch blocks before the genesis.
            self.fetch_blocks(ctx, &mut payload_queue, Some(genesis.fork.first_block))
                .await?;
            // Monitor the genesis of the main node.
            // If it changes, it means that a hard fork occurred and we need to reset the consensus state.
            s.spawn_bg::<()>(async {
                let old = genesis;
                loop {
                    if let Ok(new) = self.fetch_genesis(ctx).await {
                        if new != old {
                            return Err(anyhow::format_err!(
                                "genesis changed: old {old:?}, new {new:?}"
                            )
                            .into());
                        }
                    }
                    ctx.sleep(time::Duration::seconds(5)).await?;
                }
            });

            // Run consensus component.
            let (block_store, runner) = self
                .store
                .clone()
                .into_block_store(ctx, Some(payload_queue))
                .await
                .wrap("into_block_store()")?;
            s.spawn_bg(async { Ok(runner.run(ctx).await?) });
            let executor = executor::Executor {
                config: p2p.clone(),
                block_store,
                validator: None,
            };
            executor.run(ctx).await?;
            Ok(())
        })
        .await;
        match res {
            Ok(()) | Err(ctx::Error::Canceled(_)) => Ok(()),
            Err(ctx::Error::Internal(err)) => Err(err),
        }
    }

    /// Task fetching miniblocks using json RPC endpoint of the main node.
    pub async fn run_centralized(
        self,
        ctx: &ctx::Ctx,
        actions: ActionQueueSender,
    ) -> anyhow::Result<()> {
        let res: ctx::Result<()> = scope::run!(ctx, |ctx, s| async {
            // Update sync state in the background.
            s.spawn_bg(self.fetch_state_loop(ctx));
            let mut payload_queue = self
                .store
                .access(ctx)
                .await
                .wrap("access()")?
                .new_payload_queue(ctx, actions)
                .await
                .wrap("new_fetcher_cursor()")?;
            self.fetch_blocks(ctx, &mut payload_queue, None).await
        })
        .await;
        match res {
            Ok(()) | Err(ctx::Error::Canceled(_)) => Ok(()),
            Err(ctx::Error::Internal(err)) => Err(err),
        }
    }

    /// Periodically fetches the head of the main node
    /// and updates `SyncState` accordingly.
    async fn fetch_state_loop(&self, ctx: &ctx::Ctx) -> ctx::Result<()> {
        const DELAY_INTERVAL: time::Duration = time::Duration::milliseconds(500);
        const RETRY_INTERVAL: time::Duration = time::Duration::seconds(5);
        loop {
            match ctx.wait(self.client.fetch_l2_block_number()).await? {
                Ok(head) => {
                    self.sync_state.set_main_node_block(head);
                    ctx.sleep(DELAY_INTERVAL).await?;
                }
                Err(err) => {
                    tracing::warn!("main_node_client.fetch_l2_block_number(): {err}");
                    ctx.sleep(RETRY_INTERVAL).await?;
                }
            }
        }
    }

    /// Fetches genesis from the main node.
    async fn fetch_genesis(&self, ctx: &ctx::Ctx) -> ctx::Result<validator::Genesis> {
        let genesis = ctx
            .wait(self.client.fetch_consensus_genesis())
            .await?
            .context("fetch_consensus_genesis()")?
            .context("main node is not running consensus component")?;
        Ok(zksync_protobuf::serde::deserialize(&genesis.0).context("deserialize(genesis)")?)
    }

    /// Fetches (with retries) the given block from the main node.
    async fn fetch_block(&self, ctx: &ctx::Ctx, n: MiniblockNumber) -> ctx::Result<FetchedBlock> {
        const RETRY_INTERVAL: time::Duration = time::Duration::seconds(5);

        loop {
            let res = ctx.wait(self.client.fetch_l2_block(n, true)).await?;
            match res {
                Ok(Some(block)) => return Ok(block.try_into()?),
                Ok(None) => {}
                Err(err) if err.is_transient() => {}
                Err(err) => {
                    return Err(anyhow::format_err!("client.fetch_l2_block({}): {err}", n).into());
                }
            }
            ctx.sleep(RETRY_INTERVAL).await?;
        }
    }

    /// Fetches blocks from the main node in range `[cursor.next()..end)`.
    pub(super) async fn fetch_blocks(
        &self,
        ctx: &ctx::Ctx,
        queue: &mut storage::PayloadQueue,
        end: Option<validator::BlockNumber>,
    ) -> ctx::Result<()> {
        const MAX_CONCURRENT_REQUESTS: usize = 30;
        let first = queue.next();
        let mut next = first;
        scope::run!(ctx, |ctx, s| async {
            let (send, mut recv) = ctx::channel::bounded(MAX_CONCURRENT_REQUESTS);
            s.spawn(async {
                let send = send;
                while end.map_or(true, |end| next < end) {
                    let n = MiniblockNumber(next.0.try_into().unwrap());
                    self.sync_state.wait_for_main_node_block(ctx, n).await?;
                    send.send(ctx, s.spawn(self.fetch_block(ctx, n))).await?;
                    next = next.next();
                }
                Ok(())
            });
            while end.map_or(true, |end| queue.next() < end) {
                let block = recv.recv(ctx).await?.join(ctx).await?;
                queue.send(block).await?;
            }
            Ok(())
        })
        .await?;
        // If fetched anything, wait for the last block to be stored persistently.
        if first < queue.next() {
            self.store
                .wait_for_payload(ctx, queue.next().prev().unwrap())
                .await?;
        }
        Ok(())
    }
}
