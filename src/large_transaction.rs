use anyhow::Result;
use log::{error, info, warn};
use rayon::prelude::*;
use rayon_core::ThreadPoolBuilder;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcBlockConfig;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status::{
    EncodedTransaction, TransactionDetails, UiConfirmedBlock, UiMessage, UiTransactionEncoding,
};
use object_pool::{Pool, Reusable};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone)]
pub struct LargeTransaction {
    pub slot: u64,
    pub wallet_address: Pubkey,
    pub fee: u64,
    pub signature: String,
}

// 保持使用mimalloc，避免额外依赖
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub async fn monitor_large_transactions(
    client: Arc<RpcClient>,
    last_checked_slot: Arc<Mutex<u64>>,
) -> Result<Vec<LargeTransaction>> {
    info!("开始检查 Solana 区块链上的大额交易...");

    // 健康检查 - 减少超时时间
    timeout(Duration::from_secs(3), client.get_health())
        .await
        .map_err(|_| anyhow::anyhow!("RPC 健康检查超时"))?
        .map_err(|e| anyhow::anyhow!("RPC 连接异常: {}", e))?;

    let current_slot = timeout(Duration::from_secs(5), client.get_slot())
        .await
        .map_err(|_| anyhow::anyhow!("获取槽位超时"))?
        .map_err(|e| anyhow::anyhow!("获取槽位失败: {}", e))?;

    let mut last_slot = last_checked_slot.lock().await;
    if current_slot <= *last_slot {
        info!(
            "没有新槽位需要检查。当前槽位: {}, 上次检查: {}",
            current_slot, *last_slot
        );
        return Ok(Vec::new());
    }

    // 针对M系列Mac优化：增加批次大小，利用更多核心
    let max_slots_per_batch = 20;
    let start_slot = *last_slot + 1;
    let actual_end_slot = if current_slot - start_slot + 1 > max_slots_per_batch {
        warn!(
            "槽位数量过多 ({}), 限制为最近 {} 个槽位",
            current_slot - start_slot + 1,
            max_slots_per_batch
        );
        current_slot.saturating_sub(max_slots_per_batch - 1)
    } else {
        start_slot
    };

    // 针对M系列Mac优化线程配置
    let num_threads = std::thread::available_parallelism()?.get();
    let _thread_pool = ThreadPoolBuilder::new()
        .num_threads(num_threads.min(12)) // M系列Mac通常8-12核心，限制最大线程数
        .stack_size(2 * 1024 * 1024) // 2MB栈大小，适合M系列Mac
        .build()
        .map_err(|e| anyhow::anyhow!("创建线程池失败: {}", e))?;

    // 增加对象池大小，减少内存分配
    let pool = Arc::new(Pool::new(2000, || LargeTransaction {
        slot: 0,
        wallet_address: Pubkey::default(),
        fee: 0,
        signature: String::with_capacity(88),
    }));

    // 增加通道缓冲区大小
    let (tx, mut rx) = mpsc::channel::<(u64, UiConfirmedBlock)>(max_slots_per_batch as usize * 2);

    // 并发获取区块数据
    let mut tasks = Vec::with_capacity((current_slot - actual_end_slot + 1) as usize);
    
    for slot in actual_end_slot..=current_slot {
        let client = Arc::clone(&client);
        let tx = tx.clone();
        
        let task = tokio::spawn(async move {
            let block_config = RpcBlockConfig {
                encoding: Some(UiTransactionEncoding::Json),
                transaction_details: Some(TransactionDetails::Full),
                rewards: Some(false),
                commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            };
            
            match timeout(Duration::from_secs(20), client.get_block_with_config(slot, block_config)).await {
                Ok(Ok(block_response)) => {
                    // 注意：这里直接发送整个Response，不是block.value
                    if let Err(e) = tx.send((slot, block_response)).await {
                        error!("发送槽位 {} 的区块数据失败: {}", slot, e);
                    }
                }
                Ok(Err(e)) => error!("获取槽位 {} 的区块失败: {}", slot, e),
                Err(_) => error!("获取槽位 {} 的区块超时", slot),
            }
        });
        
        tasks.push(task);
    }

    drop(tx);

    // 预分配结果向量容量
    let mut large_transactions = Vec::with_capacity(1000);
    
    // 收集所有区块数据（异步方式）
    let mut processed_blocks = Vec::new();
    while let Some((slot, block_response)) = rx.recv().await {
        processed_blocks.push((slot, block_response));
    }

    // 并行处理所有区块
    let all_transactions: Vec<_> = processed_blocks
        .into_par_iter()
        .map(|(slot, block)| process_single_slot_block(slot, block, &pool))
        .collect();

    // 合并结果
    for slot_transactions in all_transactions {
        large_transactions.extend(slot_transactions);
    }

    // 等待所有异步任务完成
    for task in tasks {
        let _ = task.await;
    }

    *last_slot = current_slot;
    
    // 减少等待时间
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    Ok(large_transactions)
}

fn process_single_slot_block(
    slot: u64,
    block: UiConfirmedBlock,
    pool: &Arc<Pool<LargeTransaction>>,
) -> Vec<LargeTransaction> {
    const THRESHOLD: u64 = 10_000_000;
    
    let transactions = block.transactions.unwrap_or_default();
    if transactions.is_empty() {
        return Vec::new();
    }

    // 使用并行迭代器处理交易
    transactions
        .into_par_iter()
        .chunks(50) // 减少块大小以更好地平衡负载
        .flat_map(|chunk| {
            chunk
                .into_par_iter()
                .filter_map(|tx| {
                    let meta = tx.meta.as_ref()?;
                    if meta.fee <= THRESHOLD {
                        return None;
                    }

                    if let EncodedTransaction::Json(ui_tx) = &tx.transaction {
                        let signature = ui_tx.signatures.first()?.clone();
                        let wallet_address = match &ui_tx.message {
                            UiMessage::Raw(raw_message) => {
                                let key_str = raw_message.account_keys.first()?;
                                Pubkey::try_from(key_str.as_str()).ok()?
                            }
                            _ => return None,
                        };

                        // 使用对象池减少内存分配
                        let mut large_tx = pool.pull(|| LargeTransaction {
                            slot: 0,
                            wallet_address: Pubkey::default(),
                            fee: 0,
                            signature: String::with_capacity(88),
                        });

                        large_tx.slot = slot;
                        large_tx.wallet_address = wallet_address;
                        large_tx.fee = meta.fee;
                        large_tx.signature = signature;
                        
                        Some(large_tx.detach().1)
                    } else {
                        None
                    }
                })
        })
        .collect()
}
