use anyhow::Result;
use log::{error, info};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solana_transaction_status::{EncodedTransaction, UiMessage, UiTransactionEncoding};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::large_transaction::monitor_large_transactions;

// 跟单配置结构体
pub struct CopyTradeConfig {
    pub wallet: Keypair, // 你的钱包用于签名交易
    pub max_lamports: u64, // 最大跟单金额（lamports）
    pub target_program: Pubkey, // 目标程序（如 DEX 的 program ID）
}

// 主跟单函数，调用 monitor_large_transactions
pub async fn monitor_and_copy_trade(
    client: Arc<RpcClient>,
    last_checked_slot: Arc<Mutex<u64>>,
    copy_trade_config: &CopyTradeConfig,
) -> Result<()> {
    info!("开始监控并跟单 Solana 区块链上的大额交易...");

    // 调用 monitor_large_transactions 获取大额交易，设置超时
    let large_transactions = match tokio::time::timeout(
        tokio::time::Duration::from_secs(30),
        monitor_large_transactions(Arc::clone(&client), Arc::clone(&last_checked_slot))
    ).await {
        Ok(result) => result?,
        Err(_) => {
            error!("监控大额交易超时");
            println!("监控大额交易超时，请检查网络或 RPC 节点状态。");
            return Ok(());
        }
    };

    if !large_transactions.is_empty() {
        println!("检测到 {} 笔大额交易", large_transactions.len());
        for tx in large_transactions {
            println!(
                "检测到大额交易 - 槽位: {}, 钱包: {}, 费用: {} lamports, 签名: {}",
                tx.slot, tx.wallet_address, tx.fee, tx.signature
            );

            // 使用交易签名获取完整交易详情
            match client.get_transaction(&tx.signature.parse()?, UiTransactionEncoding::Json).await {
                Ok(transaction) => {
                    // 修正：直接处理 EncodedTransaction
                    let ui_tx = &transaction.transaction.transaction;
                    if let Err(e) = try_copy_trade(&client, copy_trade_config, ui_tx, tx.slot).await {
                        error!("跟单失败，槽位 {}，签名 {}: {}", tx.slot, tx.signature, e);
                    }
                }
                Err(e) => {
                    error!("获取交易详情失败，签名 {}: {}", tx.signature, e);
                }
            }
        }
    } else {
        println!("没有检测到大额交易");
    }

    Ok(())
}

// 跟单逻辑
async fn try_copy_trade(
    client: &RpcClient,
    config: &CopyTradeConfig,
    ui_tx: &EncodedTransaction,
    slot: u64,
) -> Result<()> {
    if let EncodedTransaction::Json(ui_tx_json) = ui_tx {
        // 检查交易是否针对目标程序
        let program_id = match &ui_tx_json.message {
            UiMessage::Raw(raw_message) => {
                raw_message
                    .account_keys
                    .get(0)
                    .and_then(|key| Pubkey::try_from(key.as_str()).ok())
                    .ok_or_else(|| anyhow::anyhow!("No program ID found"))?
            }
            UiMessage::Parsed(_) => {
                println!("Parsed 消息类型，跳过跟单，槽位: {}", slot);
                return Ok(());
            }
        };

        if program_id != config.target_program {
            println!("跳过交易：目标程序不匹配，槽位: {}", slot);
            return Ok(());
        }

        // 提取交易指令
        let instructions: Vec<Instruction> = match &ui_tx_json.message {
            UiMessage::Raw(raw_message) => {
                raw_message
                    .instructions
                    .iter()
                    .filter_map(|ix| {
                        let accounts: Vec<AccountMeta> = raw_message
                            .account_keys
                            .iter()
                            .enumerate()
                            .filter_map(|(i, key)| {
                                if ix.accounts.contains(&(i as u8)) {
                                    Some(AccountMeta {
                                        pubkey: key.parse::<Pubkey>().ok()?,
                                        is_signer: i == 0,
                                        is_writable: true,
                                    })
                                } else {
                                    None
                                }
                            })
                            .collect();
                        
                        let data = match bs58::decode(&ix.data).into_vec() {
                            Ok(decoded) => decoded,
                            Err(e) => {
                                error!("解码指令数据失败, 槽位:{}, 错误{}",slot, e);
                                return None;
                            }
                            
                        };
                        Some(Instruction {
                            program_id,
                            accounts,
                            data,
                        })
                    })
                    .collect()
            }
            UiMessage::Parsed(_) => {
                return Ok(());
            }
        };

        // 安全检查：余额限制
        let lamports = client
            .get_balance(&config.wallet.pubkey())
            .await
            .map_err(|e| anyhow::anyhow!("获取余额失败: {}", e))?;
        if lamports < config.max_lamports {
            error!("余额不足，无法跟单: {} lamports，槽位: {}", lamports, slot);
            return Ok(());
        }

        // 构建新交易
        let message = Message::new(&instructions, Some(&config.wallet.pubkey()));
        let mut transaction = Transaction::new_unsigned(message);

        // 签名交易
        transaction.sign(&[&config.wallet], client.get_latest_blockhash().await?);

        // 发送交易
        match client.send_and_confirm_transaction(&transaction).await {
            Ok(signature) => {
                println!("跟单成功，签名: {}", signature);
                println!("✅ 跟单成功！签名: {}", signature);
                Ok(())
            }
            Err(e) => {
                error!("跟单失败: {}", e);
                Err(anyhow::anyhow!("跟单失败: {}", e))
            }
        }
    } else {
        Ok(())
    }
}
