mod copy_trading;
mod large_transaction;
use anyhow::Result;
use log::{error, info};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::read_keypair_file};
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use dotenv::dotenv;
use crate::copy_trading::{monitor_and_copy_trade, CopyTradeConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    println!("程序开始启动！");
    dotenv().ok(); 
    println!("启动 Solana 大额交易监控和跟单程序...");

    // 配置 Solana RPC 客户端
    let rpc_url = env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let client = Arc::new(RpcClient::new(rpc_url));

    // 加载钱包密钥（从环境变量获取，建议使用安全方式）
    
    let wallet = read_keypair_file("keypair.json")
        .expect("无法读取钱包文件");
     /*
     let wallet_secret = env::var("WALLET_SECRET")
        .expect("请设置 WALLET_SECRET 环境变量，包含 Base58 编码的私钥");
    let wallet_bytes = bs58::decode(&wallet_secret)
        .into_vec()
        .expect("无法解码 WALLET_SECRET");
    let wallet = Keypair::from_bytes(&wallet_bytes)
        .expect("无法从 WALLET_SECRET 创建 Keypair");
      */
    

    // 配置跟单参数
    let copy_trade_config = CopyTradeConfig {
        wallet,
        max_lamports: 1_000_000, // 最大 0.001 SOL
        target_program: Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P") // 示例：Serum DEX program ID
            .expect("无效的目标程序 ID"),
    };

    // 初始化最后检查的槽位
    let last_checked_slot = Arc::new(Mutex::new(0));

    // 持续运行监控和跟单
    loop {
        println!("开始新一轮监控和跟单...");
        if let Err(e) = monitor_and_copy_trade(
            Arc::clone(&client),
            Arc::clone(&last_checked_slot),
            &copy_trade_config,
        ).await {
            error!("monitor_and_copy_trade 出错: {}", e);
        }
        // 每 10 秒运行一次，避免过载 RPC 节点
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
}