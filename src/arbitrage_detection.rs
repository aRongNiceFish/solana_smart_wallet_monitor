use anyhow::Result;
use log::{error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcBlockConfig;
use solana_transaction_status::{
    EncodedTransactionWithStatusMeta, TransactionDetails, UiTransactionEncoding,
    option_serializer::OptionSerializer, // 正确的导入路径
};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

// 套利交易特征结构
#[derive(Debug, Clone)]
pub struct ArbitragePattern {
    pub slot: u64,
    pub signature: Option<String>,
    pub fee: u64,
    pub involved_programs: Vec<String>,
    pub token_swaps: Vec<TokenSwap>,
    pub profit_estimation: Option<i64>,
    pub arbitrage_type: ArbitrageType,
    pub confidence_score: f64,
}

#[derive(Debug, Clone)]
pub struct TokenSwap {
    pub program_id: String,
    pub token_in: Option<String>,
    pub token_out: Option<String>,
    pub amount_in: Option<u64>,
    pub amount_out: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum ArbitrageType {
    DexArbitrage,
    FlashLoan,
    CrossChain,
    Liquidation,
    Unknown,
}

pub struct KnownPrograms {
    pub dex_programs: HashSet<String>,
    pub lending_programs: HashSet<String>,
    pub flash_loan_programs: HashSet<String>,
}

impl KnownPrograms {
    pub fn new() -> Self {
        let dex_programs = vec![
            "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM", // Raydium V4
            "JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB",  // Jupiter V4
            "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",  // Raydium
            "HWy1jotHpo6UqeQxx49dpYYdQB8wj9Qk9MdxwjLvDHB8",  // Orca
            "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",  // Raydium CLMM
        ].into_iter().map(String::from).collect();

        let lending_programs = vec![
            "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo",    // Solend
            "Port7uDVFXoP6NUeRQiWQtPTfq4dM4ZAnFzfTutS5CUb",  // Port Finance
            "LendZqTs7gn5CTSJU1jWKhKuVpjJGom45nnwPb2AMTi",   // Lend
        ].into_iter().map(String::from).collect();

        let flash_loan_programs = vec![
            "FL7SHLoanProgram1111111111111111111111111111",    // Flash loan program
        ].into_iter().map(String::from).collect();

        Self {
            dex_programs,
            lending_programs,
            flash_loan_programs,
        }
    }
}

pub struct ArbitrageDetector {
    known_programs: KnownPrograms,
    min_fee_threshold: u64,
    profit_estimation_enabled: bool,
}

impl ArbitrageDetector {
    pub fn new(min_fee_threshold: u64) -> Self {
        Self {
            known_programs: KnownPrograms::new(),
            min_fee_threshold,
            profit_estimation_enabled: true,
        }
    }

    pub async fn detect_arbitrage_transactions(
        &self,
        client: &RpcClient,
        slot: u64,
    ) -> Result<Vec<ArbitragePattern>> {
        info!("在槽位 {} 检测套利交易", slot);
        let config = RpcBlockConfig {
            encoding: Some(UiTransactionEncoding::Json),
            transaction_details: Some(TransactionDetails::Full),
            rewards: Some(false),
            commitment: None,
            max_supported_transaction_version: Some(0),
        };

        match client.get_block_with_config(slot, config).await {
            Ok(block) => {
                let mut patterns = Vec::new();
                if let Some(transactions) = block.transactions {
                    for tx in transactions {
                        if let Some(pattern) = self.analyze_transaction(tx, slot) {
                            patterns.push(pattern);
                        }
                    }
                }
                info!("在槽位 {} 检测到 {} 个套利模式", slot, patterns.len());
                Ok(patterns)
            }
            Err(e) => {
                error!("获取槽位 {} 的区块失败: {}", slot, e);
                Ok(Vec::new())
            }
        }
    }

    fn analyze_transaction(
        &self,
        tx: EncodedTransactionWithStatusMeta,
        slot: u64,
    ) -> Option<ArbitragePattern> {
        let signature = self.extract_signature(&tx);
        let fee = tx.meta.as_ref().map(|m| m.fee).unwrap_or(0);
        
        if fee < self.min_fee_threshold {
            return None;
        }

        let program_ids = self.extract_program_ids(&tx);
        if program_ids.is_empty() {
            return None;
        }

        let token_swaps = self.analyze_token_swaps(&tx, &program_ids);
        
        // 检查是否可能是套利交易
        let arbitrage_type = self.classify_arbitrage_type(&program_ids, &token_swaps);
        let confidence_score = self.calculate_confidence_score(&program_ids, &token_swaps, fee);
        
        // 只有置信度超过阈值才认为是套利交易
        if confidence_score < 0.3 {
            return None;
        }

        let profit_estimation = if self.profit_estimation_enabled {
            self.estimate_profit(&token_swaps, &tx)
        } else {
            None
        };

        Some(ArbitragePattern {
            slot,
            signature,
            fee,
            involved_programs: program_ids,
            token_swaps,
            profit_estimation,
            arbitrage_type,
            confidence_score,
        })
    }

    fn extract_program_ids(&self, tx: &EncodedTransactionWithStatusMeta) -> Vec<String> {
        let mut program_ids = HashSet::new();
        
        // 从日志消息中提取程序ID
        if let Some(meta) = &tx.meta {
            // 处理 OptionSerializer 类型的 log_messages
            match &meta.log_messages {
                OptionSerializer::Some(logs) => {
                    for log in logs {
                        // 查找 "Program XXX invoke" 模式
                        if log.contains("Program ") && log.contains(" invoke") {
                            if let Some(start) = log.find("Program ") {
                                let after_program = &log[start + 8..];
                                if let Some(end) = after_program.find(" invoke") {
                                    let program_id = after_program[..end].trim();
                                    if program_id.len() >= 32 { // Solana程序ID长度检查
                                        program_ids.insert(program_id.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                OptionSerializer::None | 
                OptionSerializer::Skip => {
                    // 没有日志信息
                }
            }
        }

        // 简化的方法：直接检查已知程序是否在交易中
        self.check_known_programs_in_logs(tx, &mut program_ids);

        program_ids.into_iter().collect()
    }

    fn check_known_programs_in_logs(&self, tx: &EncodedTransactionWithStatusMeta, program_ids: &mut HashSet<String>) {
        if let Some(meta) = &tx.meta {
            if let OptionSerializer::Some(logs) = &meta.log_messages {
                // 检查DEX程序
                for known_program in &self.known_programs.dex_programs {
                    for log in logs {
                        if log.contains(known_program) {
                            program_ids.insert(known_program.clone());
                            break;
                        }
                    }
                }

                // 检查借贷程序
                for known_program in &self.known_programs.lending_programs {
                    for log in logs {
                        if log.contains(known_program) {
                            program_ids.insert(known_program.clone());
                            break;
                        }
                    }
                }

                // 检查闪电贷程序
                for known_program in &self.known_programs.flash_loan_programs {
                    for log in logs {
                        if log.contains(known_program) {
                            program_ids.insert(known_program.clone());
                            break;
                        }
                    }
                }
            }
        }
    }

    fn is_known_program(&self, account_key: &str) -> bool {
        self.known_programs.dex_programs.contains(account_key) ||
        self.known_programs.lending_programs.contains(account_key) ||
        self.known_programs.flash_loan_programs.contains(account_key)
    }

    fn analyze_token_swaps(
        &self,
        tx: &EncodedTransactionWithStatusMeta,
        program_ids: &[String],
    ) -> Vec<TokenSwap> {
        let mut swaps = Vec::new();
        
        for program_id in program_ids {
            if self.known_programs.dex_programs.contains(program_id.as_str()) {
                // 尝试从交易中提取更多swap信息
                let (token_in, token_out, amount_in, amount_out) = 
                    self.extract_swap_details(tx, program_id);
                
                swaps.push(TokenSwap {
                    program_id: program_id.clone(),
                    token_in,
                    token_out,
                    amount_in,
                    amount_out,
                });
            }
        }
        swaps
    }

    fn extract_swap_details(
        &self,
        tx: &EncodedTransactionWithStatusMeta,
        program_id: &str,
    ) -> (Option<String>, Option<String>, Option<u64>, Option<u64>) {
        if let Some(meta) = &tx.meta {
            if let OptionSerializer::Some(logs) = &meta.log_messages {
                for log in logs {
                    if log.contains(program_id) && log.contains("Swap") {
                        // 假设日志包含类似 "Swap token_in: XXX, token_out: YYY, amount_in: ZZZ, amount_out: WWW"
                        // 这里需要根据具体 DEX 协议（如 Raydium）的日志格式解析
                        return (
                            Some("token_in_address".to_string()),
                            Some("token_out_address".to_string()),
                            Some(1000), // 示例值
                            Some(1200), // 示例值
                        );
                    }
                }
            }
        }
        (None, None, None, None)
    }

    fn classify_arbitrage_type(
        &self,
        program_ids: &[String],
        token_swaps: &[TokenSwap],
    ) -> ArbitrageType {
        let has_dex = program_ids.iter().any(|id| self.known_programs.dex_programs.contains(id.as_str()));
        let has_flash_loan = program_ids.iter().any(|id| self.known_programs.flash_loan_programs.contains(id.as_str()));
        let has_lending = program_ids.iter().any(|id| self.known_programs.lending_programs.contains(id.as_str()));
        let swap_count = token_swaps.len();
        let dex_count = program_ids.iter().filter(|id| self.known_programs.dex_programs.contains(id.as_str())).count();

        if has_flash_loan {
            return ArbitrageType::FlashLoan;
        }
        if has_dex && dex_count >= 2 && swap_count >= 2 {
            return ArbitrageType::DexArbitrage;
        }
        if has_lending && swap_count >= 1 {
            return ArbitrageType::Liquidation;
        }
        if has_dex && has_lending {
            return ArbitrageType::CrossChain;
        }
        ArbitrageType::Unknown
    }

    fn calculate_confidence_score(
        &self,
        program_ids: &[String],
        token_swaps: &[TokenSwap],
        fee: u64,
    ) -> f64 {
        let mut score: f64 = 0.0;
        let dex_count = program_ids.iter().filter(|id| self.known_programs.dex_programs.contains(id.as_str())).count();
        let flash_loan_count = program_ids.iter().filter(|id| self.known_programs.flash_loan_programs.contains(id.as_str())).count();
        let swap_count = token_swaps.len();
        let unique_programs = program_ids.len();

        // 多个DEX程序表明可能是套利
        if dex_count >= 2 {
            score += 0.4;
        } else if dex_count == 1 {
            score += 0.1;
        }

        // 闪电贷是套利的强信号
        if flash_loan_count > 0 {
            score += 0.4;
        }

        // 多个交换操作
        if swap_count >= 3 {
            score += 0.3;
        } else if swap_count >= 2 {
            score += 0.2;
        }

        // 高手续费表明复杂交易
        if fee > self.min_fee_threshold * 5 {
            score += 0.2;
        } else if fee > self.min_fee_threshold * 2 {
            score += 0.1;
        }

        // 程序多样性
        if unique_programs >= 3 {
            score += 0.1;
        }

        score.min(1.0) // 确保分数不超过1.0
    }

    fn estimate_profit(&self, token_swaps: &[TokenSwap], tx: &EncodedTransactionWithStatusMeta) -> Option<i64> {
        if token_swaps.is_empty() {
            return None;
        }

        // 简单的利润估算逻辑
        // 在实际应用中，需要分析代币余额变化来计算真实利润
        if let Some(meta) = &tx.meta {
            // 计算余额变化
            let balance_changes = meta.post_balances.iter()
                .zip(meta.pre_balances.iter())
                .map(|(post, pre)| *post as i64 - *pre as i64)
                .sum::<i64>();
            
            if balance_changes > 0 {
                Some(balance_changes)
            } else {
                Some(0)
            }
        } else {
            Some(0)
        }
    }

    fn extract_signature(&self, tx: &EncodedTransactionWithStatusMeta) -> Option<String> {
        // 从 EncodedTransaction 中提取签名
        match &tx.transaction {
            solana_transaction_status::EncodedTransaction::Json(ui_tx) => {
                ui_tx.signatures.first().map(|sig| sig.clone())
            }
            solana_transaction_status::EncodedTransaction::LegacyBinary(_) => {
                // 对于二进制编码的交易，无法直接提取签名
                None
            }
            solana_transaction_status::EncodedTransaction::Binary(_, _) => {
                // 对于二进制编码的交易，无法直接提取签名
                None
            }
            solana_transaction_status::EncodedTransaction::Accounts(_) => {
                // 对于账户编码的交易，无法直接提取签名
                None
            }
        }
    }

    pub fn report_arbitrage_findings(&self, findings: &[ArbitragePattern]) {
        info!("发现 {} 个套利模式", findings.len());
        for (i, finding) in findings.iter().enumerate() {
            info!("套利模式 #{}: ", i + 1);
            info!("  槽位: {}", finding.slot);
            info!("  签名: {:?}", finding.signature);
            info!("  类型: {:?}", finding.arbitrage_type);
            info!("  置信度: {:.2}", finding.confidence_score);
            info!("  手续费: {} lamports", finding.fee);
            info!("  涉及程序: {:?}", finding.involved_programs);
            info!("  交换数量: {}", finding.token_swaps.len());
            if let Some(profit) = finding.profit_estimation {
                info!("  预估利润: {} lamports", profit);
            }
        }
    }

    // 批量检测多个槽位
    pub async fn detect_batch_slots(
        &self,
        client: &RpcClient,
        start_slot: u64,
        end_slot: u64,
    ) -> Result<Vec<ArbitragePattern>> {
        let mut all_patterns = Vec::new();
        
        for slot in start_slot..=end_slot {
            match self.detect_arbitrage_transactions(client, slot).await {
                Ok(patterns) => {
                    all_patterns.extend(patterns);
                }
                Err(e) => {
                    warn!("检测槽位 {} 时出错: {}", slot, e);
                    continue;
                }
            }
            
            // 添加小延迟以避免请求过于频繁
            sleep(Duration::from_millis(100)).await;
        }
        
        Ok(all_patterns)
    }

    // 设置利润估算开关
    pub fn set_profit_estimation(&mut self, enabled: bool) {
        self.profit_estimation_enabled = enabled;
    }

    // 更新最小手续费阈值
    pub fn set_min_fee_threshold(&mut self, threshold: u64) {
        self.min_fee_threshold = threshold;
    }
}

// 在模块级别添加 run_arbitrage_detection 函数
pub async fn run_arbitrage_detection(
    client: Arc<RpcClient>,
    last_checked_slot: Arc<Mutex<u64>>,
    min_fee_threshold: u64,
) -> Result<()> {
    info!("启动套利检测，初始最小手续费阈值: {}", min_fee_threshold);
    
    let detector = ArbitrageDetector::new(min_fee_threshold);
    let mut current_slot = *last_checked_slot.lock().await;
    
    info!("从槽位 {} 开始套利检测", current_slot);
    
    // 获取最新槽位
    match client.get_slot().await {
        Ok(new_slot) => {
            if new_slot > current_slot {
                current_slot = new_slot;
                info!("更新当前槽位到: {}", current_slot);
            }
        }
        Err(e) => {
            error!("获取最新槽位失败: {}", e);
        }
    }
    
    // 检测当前槽位
    match detector.detect_arbitrage_transactions(&client, current_slot).await {
        Ok(patterns) => {
            if !patterns.is_empty() {
                info!("在槽位 {} 检测到 {} 个套利交易", current_slot, patterns.len());
                detector.report_arbitrage_findings(&patterns);
            }
        }
        Err(e) => {
            error!("检测槽位 {} 时出错: {}", current_slot, e);
        }
    }
    
    // 更新最后检查的槽位
    let mut last_slot = last_checked_slot.lock().await;
    if current_slot > *last_slot {
        *last_slot = current_slot;
        info!("更新最后检查的槽位到: {}", current_slot);
    }
    
    Ok(())
}

// 辅助函数用于创建默认配置的检测器
impl Default for ArbitrageDetector {
    fn default() -> Self {
        Self::new(5000) // 默认最小手续费5000 lamports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_programs_creation() {
        let programs = KnownPrograms::new();
        assert!(!programs.dex_programs.is_empty());
        assert!(!programs.lending_programs.is_empty());
        assert!(!programs.flash_loan_programs.is_empty());
    }

    #[test]
    fn test_arbitrage_detector_creation() {
        let detector = ArbitrageDetector::new(1000);
        assert_eq!(detector.min_fee_threshold, 1000);
        assert!(detector.profit_estimation_enabled);
    }

    #[test]
    fn test_confidence_score_calculation() {
        let detector = ArbitrageDetector::new(1000);
        let program_ids = vec![
            "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM".to_string(),
            "JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB".to_string(),
        ];
        let token_swaps = vec![
            TokenSwap {
                program_id: "test".to_string(),
                token_in: None,
                token_out: None,
                amount_in: None,
                amount_out: None,
            },
            TokenSwap {
                program_id: "test2".to_string(),
                token_in: None,
                token_out: None,
                amount_in: None,
                amount_out: None,
            },
        ];
        
        let score = detector.calculate_confidence_score(&program_ids, &token_swaps, 10000);
        assert!(score > 0.0);
        assert!(score <= 1.0);
    }
}
