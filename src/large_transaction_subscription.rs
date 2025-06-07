use anyhow::Result;
use log::{info, error};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_transaction_status::{UiTransactionEncoding, EncodedConfirmedTransactionWithStatusMeta};
use std::sync::mpsc;
use std::thread;
use tungstenite::connect;
use tungstenite::protocol::WebSocket;
use tungstenite::client::AutoStream;
use url::Url;
use serde_json::json;

// 由于solana-client库不支持直接的WebSocket订阅，我们需要使用底层的WebSocket库
// 这里使用tungstenite库来处理WebSocket连接

pub fn monitor_large_transactions_with_subscription(client: &RpcClient, ws_url: &str) -> Result<()> {
    info!("Connecting to Solana WebSocket at: {}", ws_url);
    
    // 创建一个通道用于接收交易数据
    let (tx, rx) = mpsc::channel();
    
    // 建立WebSocket连接
    let (mut socket, response) = connect(Url::parse(ws_url)?)?;
    info!("WebSocket connection established: {:?}", response);
    
    // 订阅所有交易
    let subscription_msg = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "transactionSubscribe",
        "params": [
            {
                "commitment": "confirmed"
            }
        ]
    });
    socket.write_message(tungstenite::Message::Text(subscription_msg.to_string()))?;
    
    // 在一个新线程中处理WebSocket接收
    thread::spawn(move || {
        loop {
            match socket.read_message() {
                Ok(msg) => {
                    if let tungstenite::Message::Text(text) = msg {
                        // 解析收到的消息
                        if let Ok(json_data) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(params) = json_data.get("params") {
                                if let Some(result) = params.get("result") {
                                    // 提取交易签名
                                    if let Some(signature) = result.get("signature") {
                                        if let Some(signature_str) = signature.as_str() {
                                            if tx.send(signature_str.to_string()).is_err() {
                                                error!("Failed to send transaction signature through channel");
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("WebSocket read error: {}", e);
                    break;
                }
            }
        }
    });
    
    // 主线程处理接收到的交易签名
    while let Ok(signature_str) = rx.recv() {
        info!("Received new transaction signature: {}", signature_str);
        // 获取交易详情
        let signature = match solana_sdk::signature::Signature::from_str(&signature_str) {
            Ok(sig) => sig,
            Err(e) => {
                error!("Invalid signature format: {}", e);
                continue;
            }
        };
        
        let config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        
        match client.get_transaction_with_config(&signature, config) {
            Ok(tx) => {
                if let Some(meta) = tx.transaction.meta {
                    if let Some(fee) = meta.fee {
                        let threshold = 10_000_000_000; // 10 SOL in lamports
                        if fee > threshold {
                            info!("Large transaction detected: Fee = {} lamports, Signature = {}", fee, signature_str);
                            // TODO: Implement alerting for large transactions
                        }
                    }
                }
            }
            Err(e) => error!("Failed to fetch transaction details for {}: {}", signature_str, e),
        }
    }
    
    Ok(())
}
