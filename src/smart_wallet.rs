use anyhow::Result;
use log::{info, error};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Signature,
};
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding};
use solana_client::rpc_config::RpcTransactionConfig;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use std::str::FromStr;
use std::sync::Arc;

/// 监控多个钱包地址是否有潜在的早期代币交易机会
pub async fn monitor_smart_wallets(client: &Arc<RpcClient>, smart_wallets: &[String]) -> Result<()> {
    info!("Starting a new monitoring cycle for {} wallets.", smart_wallets.len());

    for (index, wallet) in smart_wallets.iter().enumerate() {
        info!("Checking transactions for wallet {} of {}: {}", index + 1, smart_wallets.len(), wallet);

        // 转换钱包地址为Pubkey
        let wallet_pubkey = match Pubkey::from_str(wallet) {
            Ok(pubkey) => pubkey,
            Err(_) => {
                error!("Invalid wallet address: {}", wallet);
                continue;
            }
        };

        // 正确构造配置对象
        let config = GetConfirmedSignaturesForAddress2Config {
            before: None,
            until: None,
            limit: Some(10), // 最多获取10条
            commitment: Some(CommitmentConfig::confirmed()),
        };

        // 获取签名列表
        match client.get_signatures_for_address_with_config(&wallet_pubkey, config).await {
            Ok(signatures) => {
                if signatures.is_empty() {
                    info!("No transactions found for wallet: {}", wallet);
                } else {
                    info!("New transactions detected for wallet: {} (Total: {})", wallet, signatures.len());
                    for (tx_index, signature_info) in signatures.iter().take(5).enumerate() {
                        info!("Processing transaction {} of {} for wallet {}", tx_index + 1, signatures.len().min(5), wallet);

                        // 转换为 Signature 类型
                        let signature = match Signature::from_str(&signature_info.signature) {
                            Ok(sig) => sig,
                            Err(_) => {
                                error!("Invalid signature: {}", signature_info.signature);
                                continue;
                            }
                        };

                        // 配置交易详情请求参数
                        let tx_config = RpcTransactionConfig {
                            encoding: Some(UiTransactionEncoding::Json),
                            commitment: Some(CommitmentConfig::confirmed()),
                            max_supported_transaction_version: Some(0),
                        };

                        info!("Fetching transaction details for signature: {}", signature);

                        match client.get_transaction_with_config(&signature, tx_config).await {
                            Ok(tx) => {
                                info!("Transaction details for {}: Block time: {:?}", signature, tx.block_time);
                                let is_token_opportunity = analyze_transaction_for_token_opportunity(&tx);
                                if is_token_opportunity {
                                    info!("🚀 Potential early token opportunity detected in transaction {} for wallet: {}", signature, wallet);
                                    // TODO: Implement alerting mechanism
                                } else {
                                    info!("No token opportunity detected in transaction {}.", signature);
                                }
                            }
                            Err(e) => {
                                error!("Failed to fetch transaction details for {}: {}", signature, e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to fetch transactions for wallet {}: {}", wallet, e);
            }
        }
    }

    Ok(())
}

/// 简单分析交易是否可能是代币交易（待实现）
fn analyze_transaction_for_token_opportunity(
    tx: &EncodedConfirmedTransactionWithStatusMeta,
) -> bool {
    let transaction = &tx.transaction;
    info!("Analyzing transaction version: {:?}", transaction.version);

    // 这里只是打印信息，未来可以解析 message 调用的 program_id 是否是 DEX 或 token 程序
    let encoded_tx = &transaction.transaction;
    info!("Transaction content: {:?}", encoded_tx);

    // TODO: 进一步解析 encoded_tx，如果发现调用了 Serum / Raydium 等则返回 true
    false
}
