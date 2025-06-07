use anyhow::Result;
use log::{info, error};
use std::fs;

pub fn read_wallet_file(file_path: &str) -> Result<Vec<String>> {
    let content = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(e) => {
            error!("Failed to read wallet file: {}", e);
            return Ok(vec![]);
        }
    };
    
    let wallets: Vec<String> = content
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
        
    if wallets.is_empty() {
        error!("Wallet file is empty or contains no valid addresses.");
    } else {
        info!("Loaded {} smart wallet addresses from file.", wallets.len());
    }
    
    Ok(wallets)
}
