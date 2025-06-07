// 1. 首先添加更详细的日志和错误处理
use anyhow::Result;
use log::{debug, error, info, warn};
use rayon::{prelude::*, ThreadPoolBuilder};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcBlockConfig;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status::{
    EncodedTransaction, TransactionDetails, UiMessage, UiTransactionEncoding,
};
use std::result::Result::{Err, Ok};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct LargeTransaction {
    pub slot: u64,
    pub wallet_address: Pubkey,
    pub fee: u64,
    pub signature: String,
}

// 添加超时处理的改进版本
pub async fn monitor_large_transactions(
    client: Arc<RpcClient>,
    last_checked_slot: Arc<Mutex<u64>>,
) -> Result<Vec<LargeTransaction>> {
    info!("开始检查 Solana 区块链上的大额交易...");

    // 1. 先测试 RPC 连接
    debug!("测试 RPC 连接...");
    let health_check = timeout(Duration::from_secs(5), client.get_health()).await;
    match health_check {
        Ok(Ok(_)) => info!("RPC 连接正常"),
        Ok(Err(e)) => {
            error!("RPC 健康检查失败: {}", e);
            return Err(anyhow::anyhow!("RPC 连接异常: {}", e));
        }
        Err(_) => {
            error!("RPC 健康检查超时");
            return Err(anyhow::anyhow!("RPC 连接超时"));
        }
    }

    let mut large_transactions = Vec::new();

    // 2. 获取当前槽位，添加超时处理
    let current_slot = match timeout(Duration::from_secs(10), client.get_slot()).await {
        Ok(Ok(slot)) => {
            info!("成功获取当前槽位: {}", slot);
            slot
        }
        Ok(Err(e)) => {
            error!("获取当前槽位失败: {}", e);
            return Err(anyhow::anyhow!("获取槽位失败: {}", e));
        }
        Err(_) => {
            error!("获取当前槽位超时");
            return Err(anyhow::anyhow!("获取槽位超时"));
        }
    };

    let mut last_slot = last_checked_slot.lock().await;

    if current_slot <= *last_slot {
        info!(
            "没有新槽位需要检查。当前槽位: {}, 上次检查: {}",
            current_slot, *last_slot
        );
        return Ok(large_transactions);
    }

    let start_slot = *last_slot + 1;
    let slots_to_check = current_slot - start_slot + 1;
    info!(
        "需要检查槽位范围: {} 到 {} (共 {} 个槽位)",
        start_slot, current_slot, slots_to_check
    );

    // 3. 如果槽位太多，限制检查范围
    let max_slots_per_batch = 10; // 减少批处理大小
    let actual_end_slot = if slots_to_check > max_slots_per_batch {
        warn!(
            "槽位数量过多 ({}), 限制为最近 {} 个槽位",
            slots_to_check, max_slots_per_batch
        );
        current_slot.saturating_sub(max_slots_per_batch - 1)
    } else {
        start_slot
    };

    let block_config = RpcBlockConfig {
        encoding: Some(UiTransactionEncoding::Json),
        transaction_details: Some(TransactionDetails::Full),
        rewards: Some(false),
        commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    //创建线程池
    let pool = ThreadPoolBuilder::new().num_threads(num_cpus::get())
        .build().map_err(|e| anyhow::anyhow!("创建线程池失败:{}", e))?;
    //创建通道用于收集结果
    let (tx, rx) : (
        std::sync::mpsc::Sender<Vec<LargeTransaction>>, 
        std::sync::mpsc::Receiver<Vec<LargeTransaction>>
        ) = std::sync::mpsc::channel();
    // 逐个槽位，分配到线程池
    let mut tasks = Vec::new();
    for slot in actual_end_slot..=current_slot {
        let client = Arc::clone(&client);
        let block_config = block_config.clone();
        let tx = tx.clone();

        tasks.push(pool.spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let result:Result<Vec<LargeTransaction>, anyhow::Error> = rt.block_on(async {
                info!("正在处理槽位: {}", slot);
                let slot_result = timeout(
                    Duration::from_secs(30),
                    process_single_slot(Arc::clone(&client), slot, block_config),
                )
                .await;

                match slot_result {
                    Ok(Ok(slot_transactions)) => {
                        info!(
                            "槽位 {} 处理完成，找到 {} 个大额交易",
                            slot,
                            slot_transactions.len()
                        );
                        Ok(slot_transactions)
                    }
                    Ok(Err(e)) => {
                        warn!("处理槽位 {} 时出错: {}", slot, e);
                        Ok(Vec::new())
                    }
                    Err(_) => {
                        error!("处理槽位 {} 超时", slot);
                        Ok(Vec::new())
                    }
                }
            });

            // 发送结果到通道
            if let Ok(transactions) = result {
                let _ = tx.send(transactions);
            }
        }
        ))
    };
    drop(tx);
    while let Ok(transactions) = rx.recv() {
        large_transactions.extend(transactions);
    }

    // 7. 更新last_checked_slot
    *last_slot = current_slot;

    // 8. 添加延迟以避免频繁请求
    sleep(Duration::from_millis(500)).await;
    Ok(large_transactions)
    
}

// 单独处理一个槽位的函数
async fn process_single_slot(
    client: Arc<RpcClient>,
    slot: u64,
    block_config: RpcBlockConfig,
) -> Result<Vec<LargeTransaction>> {
    let threshold = 100000; // lamports
    let mut slot_transactions = Vec::new();

    debug!("开始获取槽位 {} 的区块数据", slot);

    match client.get_block_with_config(slot, block_config).await {
        Ok(block) => {
            debug!("成功获取槽位 {} 的区块数据", slot);

            if let Some(transactions) = block.transactions {
                debug!("槽位 {} 包含 {} 个交易", slot, transactions.len());

                let large_transactions: Vec<LargeTransaction> = transactions
                    .par_iter()
                    .enumerate()
                    .filter_map(|(tx_index, tx)| {
                        if let Some(meta) = &tx.meta {
                            if meta.fee > threshold {
                                if let EncodedTransaction::Json(ui_tx) = &tx.transaction {
                                    let signature = ui_tx.signatures.get(0)?.to_string();
                                    let wallet_address = match &ui_tx.message {
                                        UiMessage::Raw(raw_message) => raw_message
                                            .account_keys
                                            .get(0)
                                            .and_then(|key| Pubkey::try_from(key.as_str()).ok())?,
                                        _ => return None,
                                    };
                                    Some(LargeTransaction {
                                        slot,
                                        wallet_address,
                                        fee: meta.fee,
                                        signature,
                                    })
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect();
                slot_transactions.extend(large_transactions);
            } else {
                debug!("槽位 {} 没有交易数据", slot);
            }
        }
        Err(e) => {
            error!("获取槽位 {} 的区块失败: {}", slot, e);
            return Err(anyhow::anyhow!("获取区块失败: {}", e));
        }
    }

    Ok(slot_transactions)
}
