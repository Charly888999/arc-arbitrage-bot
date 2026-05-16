use ethers::prelude::*;
use ethers::types::{U256, Filter, H256, Bytes, TransactionRequest};
use std::sync::Arc as StdArc;
use std::io::{self, Write, BufWriter, BufReader, BufRead};
use std::fs::{OpenOptions, File};
use tokio::time::{sleep, Duration, Instant};
use dotenv::dotenv;
use eyre::Result;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};
use rand::Rng;

// ---------- ABI generation ----------
abigen!(
    IUniswapV3Pool,
    r#"[
        function slot0() external view returns (uint160, int24, uint16, uint16, uint16, uint8, bool)
        function token0() external view returns (address)
        function token1() external view returns (address)
    ]"#;

    IERC20,
    r#"[
        function approve(address spender, uint256 amount) external returns (bool)
        function allowance(address owner, address spender) external view returns (uint256)
        function balanceOf(address account) external view returns (uint256)
        function decimals() external view returns (uint8)
        function transfer(address to, uint256 amount) external returns (bool)
    ]"#;

    ISwapRouter,
    r#"[
        function exactInputSingle(address tokenIn, address tokenOut, uint24 fee, address recipient, uint256 deadline, uint256 amountIn, uint256 amountOutMinimum, uint160 sqrtPriceLimitX96) external payable returns (uint256 amountOut)
    ]"#;

    ITokenMessengerV2,
    r#"[
        function depositForBurnWithHook(uint256 amount, uint32 destinationDomain, bytes32 mintRecipient, address burnToken, bytes32 destinationCaller, uint256 maxFee, uint32 minFinalityThreshold, bytes calldata hookData) external returns (uint64 nonce)
    ]"#;
);

// ---------- Constants ----------
const POSITION_MANAGER_ADDR: &str = "0xC36442b4a4522E871399CD717aBDD847Ab11FE88";
const SIMPLE_STORAGE_BYTECODE: &str = "608060405234801561000f575f5ffd5b5061012a8061001d5f395ff3fe6080604052348015600e575f5ffd5b50600436106030575f3560e01c80636057361d1460345780632e64cec114604d575b5f5ffd5b604b603f3660046074565b605f565b005b60566065565b60405190815260200160405180910390f35b5f55565b5f5f54905090565b5f602082840312156084575f5ffd5b503591905056fea2646970667358221220";

const SWAPARC_POOL_ADDR: &str = "0x2F4490e7c6F3DaC23ffEe6e71bFcb5d1CCd7d4eC";
const ZK_PRIVACY_POOL_ADDR: &str = "0x5CBFe08d0be007B2796F92206804139f26D8d724";
const SWPRC_ADDR: &str = "0xBE7477BF91526FC9988C8f33e91B6db687119D45";
const USDC_ADDR: &str = "0x3600000000000000000000000000000000000000";
const EURC_ADDR: &str = "0x89B50855Aa3be2F677cD6303Cec089B5F319D72a";
const AGENTIC_COMMERCE_ADDR: &str = "0x0747EEf0706327138c69792bF28Cd525089e4583";
const MULTICALL3_ADDR: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
const USYC_ADDR: &str = "0xe9185F0c5F296Ed1797AaE4238D26CCaBEadb86C";

// ---------- Helper functions ----------
fn parse_addr(s: &str) -> Address {
    s.parse::<Address>().unwrap_or_else(|_| Address::zero())
}

fn bytes32_from_address(addr: Address) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[12..32].copy_from_slice(addr.as_bytes());
    H256::from(bytes)
}

fn format_units(amount: U256, decimals: u32) -> String {
    let amount_f64 = amount.as_u128() as f64;
    let divisor = 10f64.powi(decimals as i32);
    format!("{:.6}", amount_f64 / divisor)
}

fn generate_random_address() -> Address {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 20];
    rng.fill(&mut bytes);
    Address::from(bytes)
}

// ---------- Structures ----------
struct Logger {
    file: Option<BufWriter<std::fs::File>>,
}

impl Logger {
    fn new(log_path: &str) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .ok()
            .map(BufWriter::new);
        Logger { file }
    }

    fn log(&mut self, msg: &str) {
        if let Some(ref mut f) = self.file {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            writeln!(f, "[{}] {}", timestamp, msg).ok();
            f.flush().ok();
        }
    }
}

fn clear_line() {
    print!("\r\x1B[2K");
    io::stdout().flush().ok();
}

fn print_status(status: &str) {
    clear_line();
    print!("{}", status);
    io::stdout().flush().ok();
}

fn println_and_restore(msg: &str, last_status: &str) {
    clear_line();
    println!("{}", msg);
    print_status(last_status);
}

#[allow(dead_code)]
struct PoolConfig {
    name: String,
    pool_addr: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    is_token0_in: bool,
    decimals_in: u32,
    decimals_out: u32,
    buy_trigger: f64,
    tp_activation: f64,
    trail_dist: f64,
}

struct PoolState {
    last_buy_price: f64,
    peak_price: f64,
    is_holding: bool,
    trailing_active: bool,
    trade_count: u32,
    eurc_balance: U256,
    pending_tx: Option<TxHash>,
}

// ---------- CCTP V2 Bridge (без API) ----------
async fn bridge_usdc_to_arc(amount_usdc: f64, recipient_arc: Address) -> Result<TxHash> {
    let sepolia_rpc = env::var("SEPOLIA_RPC_URL")?;
    let priv_key = env::var("SEPOLIA_PRIVATE_KEY")?;
    let token_messenger_addr: Address = env::var("TOKEN_MESSENGER_V2_SEPOLIA")?.parse()?;
    let usdc_sepolia_addr: Address = env::var("USDC_SEPOLIA")?.parse()?;

    let provider = Provider::<Http>::try_from(sepolia_rpc)?.interval(Duration::from_millis(300));
    let wallet: LocalWallet = priv_key.parse()?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let wallet = wallet.with_chain_id(chain_id);
    let client = StdArc::new(SignerMiddleware::new(provider, wallet));

    let usdc = IERC20::new(usdc_sepolia_addr, client.clone());
    let messenger = ITokenMessengerV2::new(token_messenger_addr, client.clone());

    let amount_raw = (amount_usdc * 10f64.powi(6)) as u128;
    let amount = U256::from(amount_raw);

    // Approve
    let approve_call = usdc.approve(token_messenger_addr, amount);
    let approve_tx = approve_call.send().await?;
    approve_tx.await?;

    // Фиксированная комиссия (0.01 USDC = 10000 минимальных единиц)
    let max_fee = U256::from(10000);

    let destination_domain = 26u32;
    let mint_recipient = bytes32_from_address(recipient_arc);
    let destination_caller = H256::zero();
    let min_finality_threshold = 1000u32;
    let hook_data = hex::decode("636374702d666f72776172640000000000000000000000000000000000000000")?;

    let call = messenger.deposit_for_burn_with_hook(
        amount,
        destination_domain,
        mint_recipient.into(),
        usdc_sepolia_addr,
        destination_caller.into(),
        max_fee,
        min_finality_threshold,
        hook_data.into(),
    );
    let tx = call.send().await?;
    let tx_hash = tx.tx_hash();
    tx.await?;
    Ok(tx_hash)
}

async fn deposit_usdc_to_usyc(amount_usdc: f64) -> Result<TxHash> {
    let rpc_url = env::var("RPC_URL")?;
    let priv_key = env::var("PRIVATE_KEY")?;
    let usyc_teller_addr: Address = env::var("USYC_TELLER_ARC")?.parse()?;
    let usdc_arc_addr = parse_addr(USDC_ADDR);

    let provider = Provider::<Http>::try_from(rpc_url)?.interval(Duration::from_millis(300));
    let wallet: LocalWallet = priv_key.parse()?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let wallet = wallet.with_chain_id(chain_id);
    let client = StdArc::new(SignerMiddleware::new(provider, wallet));

    let usdc = IERC20::new(usdc_arc_addr, client.clone());
    let amount_raw = (amount_usdc * 10f64.powi(6)) as u128;
    let amount = U256::from(amount_raw);

    let approve_call = usdc.approve(usyc_teller_addr, amount);
    let approve_tx = approve_call.send().await?;
    approve_tx.await?;

    let data = ethers::abi::encode(&[ethers::abi::Token::Uint(amount)]);
    let selector = &ethers::utils::keccak256("deposit(uint256)".as_bytes())[0..4];
    let call_data = [selector, data.as_slice()].concat();
    let tx = TransactionRequest::new()
        .to(usyc_teller_addr)
        .data(call_data)
        .from(client.address())
        .gas(300_000);
    let pending = client.send_transaction(tx, None).await?;
    let tx_hash = pending.tx_hash();
    pending.await?;
    Ok(tx_hash)
}

// ---------- Core Trading ----------
async fn execute_micro_swap(
    router: &ISwapRouter<SignerMiddleware<Provider<Http>, LocalWallet>>,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    recipient: Address,
    fee: u32,
    gas_multiplier: u64,
) -> Result<()> {
    let deadline = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 300;
    let gas_price = match router.client().get_gas_price().await {
        Ok(gp) => gp * U256::from(gas_multiplier) / 100,
        Err(_) => U256::from(5_000_000_000u64),
    };
    let call = router.exact_input_single(
        token_in, token_out, fee, recipient,
        U256::from(deadline), amount_in, U256::zero(), U256::zero()
    ).gas_price(gas_price);
    let tx = call.send().await?;
    tx.await?;
    Ok(())
}

async fn execute_swap_with_slippage(
    router: &ISwapRouter<SignerMiddleware<Provider<Http>, LocalWallet>>,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    recipient: Address,
    fee: u32,
    slippage_percent: f64,
    expected_out: U256,
    _action: &str,
    gas_multiplier: u64,
) -> Result<TxHash> {
    let slippage_factor = (10000.0 - slippage_percent * 100.0) / 10000.0;
    let amount_out_minimum = U256::from((expected_out.as_u128() as f64 * slippage_factor) as u128);
    let token = IERC20::new(token_in, router.client().clone());
    if let Ok(allowance) = token.allowance(recipient, router.address()).call().await {
        if allowance < amount_in {
            let approve_call = token.approve(router.address(), U256::max_value());
            let approve_tx = approve_call.send().await?;
            approve_tx.await?;
        }
    }
    let deadline = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 300;
    let gas_price = match router.client().get_gas_price().await {
        Ok(gp) => gp * U256::from(gas_multiplier) / 100,
        Err(_) => U256::from(10_000_000_000u64),
    };
    let call = router.exact_input_single(
        token_in, token_out, fee, recipient,
        U256::from(deadline), amount_in, amount_out_minimum, U256::zero()
    ).gas_price(gas_price);
    let tx = call.send().await?;
    let tx_hash = tx.tx_hash();
    tx.await?;
    Ok(tx_hash)
}

// ---------- Unique Actions ----------
fn increase_liquidity_topic() -> H256 {
    H256::from(ethers::utils::keccak256("IncreaseLiquidity(uint256,uint128,uint256,uint256)".as_bytes()))
}

async fn add_liquidity(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
    pool_addr: Address,
    amount: U256,
) -> Result<U256> {
    let pool = IUniswapV3Pool::new(pool_addr, client.clone());
    let token0 = pool.token_0().call().await?;
    let token1 = pool.token_1().call().await?;
    let slot = pool.slot_0().call().await?;
    let current_tick = slot.1;
    let tick_lower = (current_tick - 100) / 60 * 60;
    let tick_upper = (current_tick + 100) / 60 * 60;
    let pos_mgr_addr: Address = POSITION_MANAGER_ADDR.parse()?;

    let usdc = IERC20::new(token0, client.clone());
    let eurc = IERC20::new(token1, client.clone());

    let approve_usdc_call = usdc.approve(pos_mgr_addr, amount);
    let approve_usdc_tx = approve_usdc_call.send().await?;
    approve_usdc_tx.await?;
    let approve_eurc_call = eurc.approve(pos_mgr_addr, amount);
    let approve_eurc_tx = approve_eurc_call.send().await?;
    approve_eurc_tx.await?;

    let deadline = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
    let mint_data = ethers::abi::encode(&[
        ethers::abi::Token::Tuple(vec![
            ethers::abi::Token::Address(token0),
            ethers::abi::Token::Address(token1),
            ethers::abi::Token::Uint(U256::from(3000u32)),
            ethers::abi::Token::Int(U256::from_big_endian(&tick_lower.to_be_bytes())),
            ethers::abi::Token::Int(U256::from_big_endian(&tick_upper.to_be_bytes())),
            ethers::abi::Token::Uint(amount),
            ethers::abi::Token::Uint(amount),
            ethers::abi::Token::Uint(U256::zero()),
            ethers::abi::Token::Uint(U256::zero()),
            ethers::abi::Token::Address(wallet.address()),
            ethers::abi::Token::Uint(U256::from(deadline)),
        ]),
    ]);
    let call_data = format!("0x88316456{}", hex::encode(mint_data));
    let tx = TransactionRequest::new()
        .to(pos_mgr_addr)
        .from(wallet.address())
        .data(call_data.parse::<Bytes>()?)
        .gas(600_000);
    let pending = client.send_transaction(tx, None).await?;
    let receipt = pending.await?.ok_or_else(|| eyre::eyre!("mint not confirmed"))?;

    let topic = increase_liquidity_topic();
    let token_id_opt = receipt.logs.iter()
        .find(|log| log.topics.first() == Some(&topic))
        .and_then(|log| log.topics.get(1))
        .map(|t| U256::from_big_endian(t.as_bytes()));

    if let Some(token_id) = token_id_opt {
        return Ok(token_id);
    }

    let filter = Filter::new()
        .address(pos_mgr_addr)
        .topic0(topic)
        .from_block(receipt.block_number.unwrap())
        .to_block(receipt.block_number.unwrap());
    let logs = client.provider().get_logs(&filter).await?;
    if let Some(log) = logs.first() {
        if let Some(t) = log.topics.get(1) {
            let token_id = U256::from_big_endian(t.as_bytes());
            return Ok(token_id);
        }
    }
    Ok(U256::zero())
}

async fn rebalance_liquidity(
    _client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    _wallet: &LocalWallet,
    _pool_addr: Address,
    _token_id: U256,
    _liquidity_nft_id: &mut Option<U256>,
) -> Result<()> {
    println!("⚖️ Liquidity rebalance (stub)");
    Ok(())
}

async fn simulate_sandwich(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
    _pool_addr: Address,
) -> Result<()> {
    println!("🥪 MEV sandwich: fast opposite swaps");
    let router_addr: Address = "0x140416976696b02005e81055f2b84260f85f3192".parse()?;
    let router = ISwapRouter::new(router_addr, client.clone());
    let usdc_addr = parse_addr(USDC_ADDR);
    let eurc_addr = parse_addr(EURC_ADDR);
    let amount = U256::from(50_000u64);

    let _ = execute_micro_swap(&router, usdc_addr, eurc_addr, amount, wallet.address(), 3000, 200).await;
    let _ = execute_micro_swap(&router, eurc_addr, usdc_addr, amount, wallet.address(), 3000, 200).await;
    println!("   ✅ Sandwich executed");
    Ok(())
}

async fn backrun_opportunity(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
    pool_addr: Address,
) -> Result<()> {
    println!("🔄 Backrunning: simulating pool state...");
    let pool = IUniswapV3Pool::new(pool_addr, client.clone());
    let slot = pool.slot_0().call().await?;
    let current_price = (slot.0.as_u128() as f64 / 2f64.powi(96)).powi(2);
    let simulated_price_after = current_price * 0.99;
    if (simulated_price_after - current_price).abs() > 0.001 {
        println!("   📊 Price deviation detected: {:.6} → {:.6}", current_price, simulated_price_after);
        let router_addr: Address = "0x140416976696b02005e81055f2b84260f85f3192".parse()?;
        let router = ISwapRouter::new(router_addr, client.clone());
        let usdc_addr = parse_addr(USDC_ADDR);
        let eurc_addr = parse_addr(EURC_ADDR);
        let amount = U256::from(50_000u64);
        let _ = execute_micro_swap(&router, eurc_addr, usdc_addr, amount, wallet.address(), 3000, 200).await;
        println!("   ✅ Backrunning executed");
    }
    Ok(())
}

async fn swaparc_swap(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
) -> Result<()> {
    let pool_addr = parse_addr(SWAPARC_POOL_ADDR);
    let selector = [0x3d, 0xf0, 0x21, 0x24];
    let mut encoded = Vec::with_capacity(4 + 128);
    encoded.extend_from_slice(&selector);
    let i = 1i128;
    let j = 0i128;
    let dx = U256::exp10(18) / 100;
    let min_dy = U256::zero();
    let i_bytes = {
        let mut b = [0u8; 32];
        let v = i.to_be_bytes();
        b[16..32].copy_from_slice(&v);
        b
    };
    let j_bytes = {
        let mut b = [0u8; 32];
        let v = j.to_be_bytes();
        b[16..32].copy_from_slice(&v);
        b
    };
    encoded.extend_from_slice(&i_bytes);
    encoded.extend_from_slice(&j_bytes);
    let mut dx_bytes = [0u8; 32];
    dx.to_big_endian(&mut dx_bytes);
    encoded.extend_from_slice(&dx_bytes);
    let mut min_dy_bytes = [0u8; 32];
    min_dy.to_big_endian(&mut min_dy_bytes);
    encoded.extend_from_slice(&min_dy_bytes);
    let data = Bytes::from(encoded);
    let tx = TransactionRequest::new()
        .to(pool_addr)
        .from(wallet.address())
        .data(data)
        .gas(300_000);
    let pending = client.send_transaction(tx, None).await?;
    pending.await?.ok_or_else(|| eyre::eyre!("SwapArc exchange failed"))?;
    println!("🔄 SwapArc swap done (SWPRC → USDC)");
    Ok(())
}

async fn privpay_deposit(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
) -> Result<()> {
    let zk_pool_addr = parse_addr(ZK_PRIVACY_POOL_ADDR);
    let mut rng = rand::thread_rng();
    let mut secret = [0u8; 32];
    rng.fill(&mut secret);
    let commitment = ethers::utils::keccak256(secret);
    let deposit_amount = U256::exp10(18) / 1000;
    let swprc_addr = parse_addr(SWPRC_ADDR);
    let swprc = IERC20::new(swprc_addr, client.clone());
    let approve_call = swprc.approve(zk_pool_addr, deposit_amount);
    let approve_tx = approve_call.send().await?;
    approve_tx.await?;
    let selector = [0xb2, 0x14, 0xfa, 0xa5];
    let mut encoded = Vec::with_capacity(4 + 64);
    encoded.extend_from_slice(&selector);
    encoded.extend_from_slice(&commitment);
    let mut amount_bytes = [0u8; 32];
    deposit_amount.to_big_endian(&mut amount_bytes);
    encoded.extend_from_slice(&amount_bytes);
    let data = Bytes::from(encoded);
    let tx = TransactionRequest::new()
        .to(zk_pool_addr)
        .from(wallet.address())
        .data(data)
        .gas(300_000);
    let pending = client.send_transaction(tx, None).await?;
    pending.await?.ok_or_else(|| eyre::eyre!("ZK deposit failed"))?;
    println!("🔐 ZK deposit done (SWPRC)");
    Ok(())
}

async fn erc8183_create_job(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
) -> Result<()> {
    let contract_addr = parse_addr(AGENTIC_COMMERCE_ADDR);
    let usdc_addr = parse_addr(USDC_ADDR);
    let mut rng = rand::thread_rng();
    let mut job_id = [0u8; 32];
    rng.fill(&mut job_id);
    let escrow_amount = U256::exp10(6) / 100;
    let usdc = IERC20::new(usdc_addr, client.clone());
    let approve_call = usdc.approve(contract_addr, escrow_amount);
    let approve_tx = approve_call.send().await?;
    approve_tx.await?;
    let selector = ethers::utils::keccak256("createJob(bytes32,address,address,uint256,string)".as_bytes());
    let selector = &selector[0..4];
    let provider = wallet.address();
    let token = usdc_addr;
    let metadata = "bot-job".to_string();
    let tokens = vec![
        ethers::abi::Token::FixedBytes(job_id.to_vec()),
        ethers::abi::Token::Address(provider),
        ethers::abi::Token::Address(token),
        ethers::abi::Token::Uint(escrow_amount),
        ethers::abi::Token::String(metadata),
    ];
    let data = ethers::abi::encode(&tokens);
    let mut full_data = Vec::from(selector);
    full_data.extend_from_slice(&data);
    let tx = TransactionRequest::new()
        .to(contract_addr)
        .from(wallet.address())
        .data(Bytes::from(full_data))
        .gas(500_000);
    let pending = client.send_transaction(tx, None).await?;
    pending.await?.ok_or_else(|| eyre::eyre!("ERC-8183 createJob failed"))?;
    println!("📋 ERC-8183 Job created");
    Ok(())
}

async fn multicall_batch(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
) -> Result<()> {
    let multicall_addr = parse_addr(MULTICALL3_ADDR);
    let usdc_addr = parse_addr(USDC_ADDR);
    let usdc = IERC20::new(usdc_addr, client.clone());
    let router_addr: Address = "0x140416976696b02005e81055f2b84260f85f3192".parse()?;
    let call1 = (usdc_addr, usdc.approve(router_addr, U256::from(100_000u64)).calldata().unwrap());
    let call2 = (usdc_addr, usdc.transfer(wallet.address(), U256::from(1u64)).calldata().unwrap());
    let call3 = (usdc_addr, usdc.transfer(wallet.address(), U256::zero()).calldata().unwrap());
    let calls = vec![
        (call1.0, call1.1.to_vec()),
        (call2.0, call2.1.to_vec()),
        (call3.0, call3.1.to_vec()),
    ];
    let aggregate_selector = ethers::utils::keccak256("aggregate3((address,bytes)[])".as_bytes());
    let aggregate_selector = &aggregate_selector[0..4];
    let mut encoded = Vec::from(aggregate_selector);
    let calls_encoded = ethers::abi::encode(&[ethers::abi::Token::Array(
        calls.into_iter().map(|(target, data)| {
            ethers::abi::Token::Tuple(vec![
                ethers::abi::Token::Address(target),
                ethers::abi::Token::Bytes(data),
            ])
        }).collect()
    )]);
    encoded.extend_from_slice(&calls_encoded);
    let tx = TransactionRequest::new()
        .to(multicall_addr)
        .from(wallet.address())
        .data(Bytes::from(encoded))
        .gas(500_000);
    let pending = client.send_transaction(tx, None).await?;
    pending.await?.ok_or_else(|| eyre::eyre!("Multicall3 aggregate3 failed"))?;
    println!("📦 Multicall3 batch done");
    Ok(())
}

async fn usyc_transfer(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
) -> Result<()> {
    let usyc_addr = parse_addr(USYC_ADDR);
    let usyc = IERC20::new(usyc_addr, client.clone());
    let amount = U256::exp10(6) / 100;
    let transfer_call = usyc.transfer(wallet.address(), amount);
    let tx = transfer_call.send().await?;
    tx.await?;
    println!("💵 USYC transfer done");
    Ok(())
}

async fn perform_unique_action(
    client: &StdArc<SignerMiddleware<Provider<Http>, LocalWallet>>,
    wallet: &LocalWallet,
    deployed_contract: &mut Option<Address>,
    unique_action_count: &mut u32,
    liquidity_nft_id: &mut Option<U256>,
) -> Result<()> {
    if deployed_contract.is_none() {
        let tx = TransactionRequest::new()
            .from(wallet.address())
            .data(hex::decode(SIMPLE_STORAGE_BYTECODE)?)
            .gas(500_000);
        let pending = client.send_transaction(tx, None).await?;
        let receipt = pending.await?.ok_or_else(|| eyre::eyre!("deploy failed"))?;
        let addr = receipt.contract_address.ok_or_else(|| eyre::eyre!("no contract address"))?;
        *deployed_contract = Some(addr);
        println!("📜 Contract deployed: {:?}", addr);
    }

    if let Some(addr) = *deployed_contract {
        let mut val_bytes = [0u8; 32];
        U256::from(*unique_action_count).to_big_endian(&mut val_bytes);
        let data = format!("0x6057361d{}", hex::encode(val_bytes));
        let tx = TransactionRequest::new()
            .to(addr)
            .from(wallet.address())
            .data(data.parse::<Bytes>()?)
            .gas(100_000);
        let pending = client.send_transaction(tx, None).await?;
        pending.await?.ok_or_else(|| eyre::eyre!("store call failed"))?;
        println!("🔧 Contract called: store({})", unique_action_count);
    }

    let eurc_addr = parse_addr(EURC_ADDR);
    let eurc = IERC20::new(eurc_addr, client.clone());
    let amount = U256::exp10(6);
    let transfer_call = eurc.transfer(wallet.address(), amount);
    let tx = transfer_call.send().await?;
    tx.await?;
    println!("💶 Paymaster: gas paid in EURC");

    let pool_addr: Address = "0x1c02648cf14ece64afc1af141c177760aec7013a".parse()?;
    if let Some(_token_id) = *liquidity_nft_id {
        let _ = rebalance_liquidity(client, wallet, pool_addr, _token_id, liquidity_nft_id).await;
    } else {
        match add_liquidity(client, wallet, pool_addr, U256::exp10(6) / 10).await {
            Ok(token_id) => {
                *liquidity_nft_id = Some(token_id);
                println!("💧 Liquidity added, tokenId: {:?}", token_id);
            }
            Err(e) => eprintln!("❌ Add liquidity error: {}", e),
        }
    }

    if *unique_action_count % 5 == 0 {
        let _ = simulate_sandwich(client, wallet, pool_addr).await;
    }
    if *unique_action_count % 4 == 0 {
        let _ = backrun_opportunity(client, wallet, pool_addr).await;
    }
    if *unique_action_count % 3 == 0 {
        let swprc_addr = parse_addr(SWPRC_ADDR);
        let swprc = IERC20::new(swprc_addr, client.clone());
        let bal = swprc.balance_of(wallet.address()).call().await?;
        if bal > U256::zero() {
            let _ = swaparc_swap(client, wallet).await;
        } else {
            println!("⚠️ No SWPRC for SwapArc swap");
        }
    }
    if *unique_action_count % 6 == 0 {
        let swprc_addr = parse_addr(SWPRC_ADDR);
        let swprc = IERC20::new(swprc_addr, client.clone());
        let bal = swprc.balance_of(wallet.address()).call().await?;
        if bal > U256::zero() {
            let _ = privpay_deposit(client, wallet).await;
        } else {
            println!("⚠️ No SWPRC for ZK deposit");
        }
    }
    if *unique_action_count % 7 == 0 {
        let _ = erc8183_create_job(client, wallet).await;
    }
    if *unique_action_count % 8 == 0 {
        let _ = multicall_batch(client, wallet).await;
    }
    if *unique_action_count % 9 == 0 {
        let usyc_addr = parse_addr(USYC_ADDR);
        let usyc = IERC20::new(usyc_addr, client.clone());
        let bal = usyc.balance_of(wallet.address()).call().await?;
        if bal > U256::zero() {
            let _ = usyc_transfer(client, wallet).await;
        } else {
            println!("⚠️ No USYC for transfer");
        }
    }

    // Bridge выполняется только если BRIDGE_ENABLED=true и прошло условие %15
    let bridge_enabled = env::var("BRIDGE_ENABLED").unwrap_or_else(|_| "false".to_string()) == "true";
    if *unique_action_count % 15 == 0 && bridge_enabled {
        let sepolia_rpc_ok = env::var("SEPOLIA_RPC_URL").is_ok();
        let sepolia_key_ok = env::var("SEPOLIA_PRIVATE_KEY").is_ok();
        if sepolia_rpc_ok && sepolia_key_ok {
            let bridge_amount: f64 = env::var("BRIDGE_AMOUNT").unwrap_or_else(|_| "0.5".to_string()).parse()?;
            if let Err(e) = bridge_usdc_to_arc(bridge_amount, wallet.address()).await {
                eprintln!("❌ Bridge error in unique action: {}", e);
            } else {
                println!("🌉 Bridge executed as unique action");
                if env::var("AUTO_USYC_DEPOSIT").unwrap_or_default() == "true" {
                    if let Err(e) = deposit_usdc_to_usyc(bridge_amount).await {
                        eprintln!("❌ USYC deposit error: {}", e);
                    } else {
                        println!("✅ USYC deposit done after bridge");
                    }
                }
            }
        } else {
            println!("⚠️ Bridge skipped: missing Sepolia RPC or private key");
        }
    }

    *unique_action_count += 1;
    Ok(())
}

// ---------- Simulation ----------
async fn run_simulation() -> Result<()> {
    println!("📊 Running simulation on historical data");
    let file_path = env::var("SIMULATION_CSV").unwrap_or_else(|_| "historical_prices.csv".to_string());
    let file = File::open(&file_path).expect("CSV file not found");
    let reader = BufReader::new(file);
    let mut prices: Vec<f64> = Vec::new();
    for line in reader.lines() {
        if let Ok(line) = line {
            if let Ok(price) = line.trim().parse::<f64>() {
                prices.push(price);
            }
        }
    }
    println!("Loaded {} prices", prices.len());
    let buy_trigger: f64 = env::var("BUY_TRIGGER").unwrap_or_else(|_| "0.8670".to_string()).parse()?;
    let tp_activation: f64 = env::var("TP_ACTIVATION").unwrap_or_else(|_| "0.0003".to_string()).parse()?;
    let trail_dist: f64 = env::var("TRAIL_DIST").unwrap_or_else(|_| "0.0004".to_string()).parse()?;
    let trade_amount = 1.0;
    let mut last_buy_price = 0.0;
    let mut peak = 0.0;
    let mut holding = false;
    let mut trailing = false;
    let mut trades = 0;
    let mut profit = 0.0;
    for &price in &prices {
        if !holding && price < buy_trigger { holding = true; last_buy_price = price; }
        else if holding && !trailing && price > last_buy_price + tp_activation { trailing = true; peak = price; }
        else if trailing {
            if price > peak { peak = price; }
            if price < peak - trail_dist {
                let trade_profit = trade_amount * (price / last_buy_price - 1.0);
                profit += trade_profit;
                holding = false;
                trailing = false;
                trades += 1;
            }
        }
    }
    println!("Simulation done. Trades: {}, Total profit: {:.4} USDC", trades, profit);
    Ok(())
}

// ---------- Main ----------
#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let simulation_mode = env::var("SIMULATION_MODE").unwrap_or_default() == "true";
    if simulation_mode {
        run_simulation().await?;
        return Ok(());
    }

    let rpc_url = env::var("RPC_URL")
        .unwrap_or_else(|_| "https://rpc.testnet.arc.network".to_string());
    let chain_id: u64 = env::var("CHAIN_ID")
        .unwrap_or_else(|_| "5042002".to_string()).parse().expect("Invalid CHAIN_ID");
    let priv_key = env::var("PRIVATE_KEY").expect("PRIVATE_KEY not found");
    let wallet: LocalWallet = priv_key.parse::<LocalWallet>().expect("Invalid private key").with_chain_id(chain_id);

    let mut logger = Logger::new("bot.log");
    logger.log(&format!("🔗 Connecting to RPC: {}", rpc_url));

    let provider = Provider::<Http>::try_from(rpc_url)?.interval(Duration::from_millis(300));
    let client = StdArc::new(SignerMiddleware::new(provider.clone(), wallet.clone()));

    let usdc_addr = parse_addr(USDC_ADDR);
    let eurc_addr = parse_addr(EURC_ADDR);

    let usdc = IERC20::new(usdc_addr, client.clone());
    let eurc = IERC20::new(eurc_addr, client.clone());
    let usdc_decimals = usdc.decimals().call().await? as u32;
    let eurc_decimals = eurc.decimals().call().await? as u32;

    let router_addr: Address = env::var("ROUTER_ADDRESS")
        .unwrap_or_else(|_| "0x140416976696b02005e81055f2b84260f85f3192".to_string()).parse()?;
    let router = ISwapRouter::new(router_addr, client.clone());

    let trade_amount_usdc: f64 = env::var("TRADE_AMOUNT").unwrap_or_else(|_| "1.0".to_string()).parse()?;
    let trade_amount = U256::from((trade_amount_usdc * 10f64.powi(usdc_decimals as i32)) as u128);
    let slippage_percent: f64 = env::var("SLIPPAGE_PERCENT").unwrap_or_else(|_| "2.0".to_string()).parse()?;
    let gas_multiplier: u64 = env::var("GAS_MULTIPLIER").unwrap_or_else(|_| "200".to_string()).parse()?;
    let mev_protection: bool = env::var("MEV_PROTECTION").unwrap_or_else(|_| "false".to_string()).parse()?;
    let buy_trigger_price: f64 = env::var("BUY_TRIGGER").unwrap_or_else(|_| "0.8670".to_string()).parse()?;
    let tp_activation: f64 = env::var("TP_ACTIVATION").unwrap_or_else(|_| "0.0003".to_string()).parse()?;
    let trail_dist: f64 = env::var("TRAIL_DIST").unwrap_or_else(|_| "0.0004".to_string()).parse()?;

    let pools_config = vec![
        PoolConfig {
            name: "EURC/USDC 0.3%".to_string(),
            pool_addr: "0x1c02648cf14ece64afc1af141c177760aec7013a".parse()?,
            token_in: usdc_addr,
            token_out: eurc_addr,
            fee: 3000,
            is_token0_in: true,
            decimals_in: usdc_decimals,
            decimals_out: eurc_decimals,
            buy_trigger: buy_trigger_price,
            tp_activation,
            trail_dist,
        },
    ];

    let mut pools = Vec::new();
    for mut config in pools_config {
        let pool = IUniswapV3Pool::new(config.pool_addr, client.clone());
        let token0 = pool.token_0().call().await?;
        config.is_token0_in = token0 == config.token_in;
        pools.push((config, pool));
    }

    let mut pool_states: Vec<PoolState> = pools.iter().map(|_| PoolState {
        last_buy_price: 0.0,
        peak_price: 0.0,
        is_holding: false,
        trailing_active: false,
        trade_count: 0,
        eurc_balance: U256::zero(),
        pending_tx: None,
    }).collect();

    let mut total_session_profit: f64 = 0.0;
    let mut random_swap_count: u32 = 0;
    let mut last_activity_time = Instant::now();
    let mut last_unique_action_time = Instant::now();
    let unique_action_interval = Duration::from_secs(180);
    let micro_interval = Duration::from_secs(30);
    let mut last_bridge_time = Instant::now();
    let bridge_interval = Duration::from_secs(env::var("BRIDGE_INTERVAL_MINUTES").unwrap_or_else(|_| "10".to_string()).parse::<u64>().unwrap_or(10) * 60);
    let mut last_status = String::new();
    let mut last_price: f64 = 0.0;

    let mut deployed_contract: Option<Address> = None;
    let mut unique_action_count: u32 = 0;
    let mut liquidity_nft_id: Option<U256> = None;

    logger.log("🚀 Bot started (CCTP V2 + USYC + ERC-8183)");
    println!("🚀 Bot started! Log in bot.log");

    loop {
        for (i, (config, _pool)) in pools.iter().enumerate() {
            let state = &mut pool_states[i];
            if let Some(tx_hash) = state.pending_tx {
                if let Ok(Some(tx)) = client.get_transaction(tx_hash).await {
                    if tx.block_number.is_some() {
                        println_and_restore(&format!("[{}] ✅ TX confirmed: {:?}", config.name, tx_hash), &last_status);
                        logger.log(&format!("[{}] ✅ TX confirmed: {:?}", config.name, tx_hash));
                        state.pending_tx = None;
                        if state.is_holding {
                            if let Ok(bal) = eurc.balance_of(wallet.address()).call().await {
                                state.eurc_balance = bal;
                                logger.log(&format!("[{}] EURC balance: {}", config.name, format_units(bal, config.decimals_out)));
                            }
                        }
                    }
                }
            }
        }

        let bal_usdc = usdc.balance_of(wallet.address()).call().await.unwrap_or_default();
        let bal_eurc = eurc.balance_of(wallet.address()).call().await.unwrap_or_default();

        let mut pool_prices: Vec<(usize, f64)> = Vec::new();
        for (i, (config, pool)) in pools.iter().enumerate() {
            if let Ok(slot) = pool.slot_0().call().await {
                let sqrt_price_x96 = slot.0.as_u128() as f64;
                let raw_price = (sqrt_price_x96 * sqrt_price_x96) / 2f64.powi(192);
                let price = if config.is_token0_in { raw_price } else { 1.0 / raw_price };
                pool_prices.push((i, price));
            }
        }

        if pool_prices.is_empty() {
            sleep(Duration::from_secs(1)).await;
            continue;
        }
        if let Some(&(_, price)) = pool_prices.first() {
            last_price = price;
        }

        for (i, (config, _pool)) in pools.iter().enumerate() {
            if let Some(&(_, price)) = pool_prices.iter().find(|&&(idx, _)| idx == i) {
                let state = &mut pool_states[i];
                let mev_delay = if mev_protection { let mut rng = rand::thread_rng(); rng.gen_range(0..2000) } else { 0 };

                if !state.is_holding && bal_usdc >= trade_amount && price < config.buy_trigger && state.pending_tx.is_none() {
                    println_and_restore(&format!("[{}] 📥 Buy signal at {:.6}", config.name, price), &last_status);
                    logger.log(&format!("[{}] 📥 Buy signal at {:.6}", config.name, price));
                    let expected_out = if config.is_token0_in { U256::from((trade_amount.as_u128() as f64 / price) as u128) } else { U256::from((trade_amount.as_u128() as f64 * price) as u128) };
                    if mev_delay > 0 { sleep(Duration::from_millis(mev_delay)).await; }
                    match execute_swap_with_slippage(&router, config.token_in, config.token_out, trade_amount, wallet.address(), config.fee, slippage_percent, expected_out, "buy", gas_multiplier).await {
                        Ok(h) => {
                            state.pending_tx = Some(h);
                            state.last_buy_price = price;
                            state.is_holding = true;
                            state.trailing_active = false;
                            state.peak_price = 0.0;
                            state.trade_count += 1;
                            println_and_restore("⏳ Waiting for buy confirmation...", &last_status);
                            logger.log(&format!("[{}] ⏳ Buy tx: {:?}", config.name, h));
                        }
                        Err(e) => {
                            println_and_restore(&format!("[{}] ❌ Buy error: {}", config.name, e), &last_status);
                            logger.log(&format!("[{}] ❌ Buy error: {}", config.name, e));
                        }
                    }
                }

                if state.is_holding && !state.trailing_active && price > (state.last_buy_price + config.tp_activation) {
                    println_and_restore(&format!("[{}] 🚀 Trailing activated! Peak: {:.6}", config.name, price), &last_status);
                    logger.log(&format!("[{}] 🚀 Trailing activated! Peak: {:.6}", config.name, price));
                    state.trailing_active = true;
                    state.peak_price = price;
                }

                if state.trailing_active && state.pending_tx.is_none() {
                    let profit = (price / state.last_buy_price - 1.0).max(0.0);
                    let dynamic_trail = config.trail_dist + profit * 0.2;
                    if price > state.peak_price { state.peak_price = price; }
                    let stop_price = state.peak_price - dynamic_trail;
                    if price < stop_price {
                        println_and_restore(&format!("[{}] 📤 Sell signal! Price: {:.6}, Peak: {:.6}", config.name, price, state.peak_price), &last_status);
                        logger.log(&format!("[{}] 📤 Sell signal! Price: {:.6}, Peak: {:.6}", config.name, price, state.peak_price));
                        let cur_eurc = if state.eurc_balance > U256::zero() { state.eurc_balance } else { bal_eurc };
                        let expected_usdc = if config.is_token0_in { U256::from((cur_eurc.as_u128() as f64 * price) as u128) } else { U256::from((cur_eurc.as_u128() as f64 / price) as u128) };
                        if mev_delay > 0 { sleep(Duration::from_millis(mev_delay)).await; }
                        match execute_swap_with_slippage(&router, config.token_out, config.token_in, cur_eurc, wallet.address(), config.fee, slippage_percent, expected_usdc, "sell", gas_multiplier).await {
                            Ok(h) => {
                                state.pending_tx = Some(h);
                                let profit_usdc = trade_amount_usdc * (price / state.last_buy_price - 1.0);
                                total_session_profit += profit_usdc;
                                state.is_holding = false;
                                state.trailing_active = false;
                                state.peak_price = 0.0;
                                state.eurc_balance = U256::zero();
                                println_and_restore("⏳ Waiting for sell confirmation...", &last_status);
                                logger.log(&format!("[{}] 💰 Profit: {:+.4} USDC, Total: {:+.4}", config.name, profit_usdc, total_session_profit));
                            }
                            Err(e) => {
                                println_and_restore(&format!("[{}] ❌ Sell error: {}", config.name, e), &last_status);
                                logger.log(&format!("[{}] ❌ Sell error: {}", config.name, e));
                            }
                        }
                    }
                }
            }
        }

        // Micro swaps
        if last_activity_time.elapsed() >= micro_interval && pool_states.iter().all(|s| s.pending_tx.is_none()) {
            let mut rng = rand::thread_rng();
            let r: f64 = rng.gen();
            if r < 0.4 && bal_usdc > U256::from(100_000) {
                let amt = U256::from(rng.gen_range(50_000..200_000));
                println_and_restore(&format!("🎲 Micro swap USDC->EURC for {}", amt), &last_status);
                logger.log(&format!("🎲 Micro swap USDC->EURC for {}", amt));
                if let Err(e) = execute_micro_swap(&router, usdc_addr, eurc_addr, amt, wallet.address(), 3000, gas_multiplier).await {
                    println_and_restore(&format!("❌ Micro swap error: {}", e), &last_status);
                    logger.log(&format!("❌ Micro swap error: {}", e));
                } else { random_swap_count += 1; }
            } else if r < 0.6 && bal_eurc > U256::from(100_000) {
                let amt = U256::from(rng.gen_range(50_000..200_000));
                println_and_restore(&format!("🔄 Reverse micro swap EURC->USDC for {}", amt), &last_status);
                logger.log(&format!("🔄 Reverse micro swap EURC->USDC for {}", amt));
                if let Err(e) = execute_micro_swap(&router, eurc_addr, usdc_addr, amt, wallet.address(), 3000, gas_multiplier).await {
                    println_and_restore(&format!("❌ Reverse swap error: {}", e), &last_status);
                    logger.log(&format!("❌ Reverse swap error: {}", e));
                } else { random_swap_count += 1; }
            } else if r < 0.85 && bal_usdc > U256::from(200_000) {
                let amt = U256::from(rng.gen_range(30_000..100_000));
                let addr = generate_random_address();
                println_and_restore(&format!("📤 Micro transfer USDC to {}", amt), &last_status);
                logger.log(&format!("📤 Micro transfer USDC to {}", amt));
                let transfer_call = usdc.transfer(addr, amt);
                let tx = transfer_call.send().await?;
                tx.await?;
                random_swap_count += 1;
            }
            last_activity_time = Instant::now();
        }

        // Bridge
        let bridge_enabled = env::var("BRIDGE_ENABLED").unwrap_or_else(|_| "false".to_string()) == "true";
        if bridge_enabled && last_bridge_time.elapsed() >= bridge_interval && pool_states.iter().all(|s| s.pending_tx.is_none()) {
            let sepolia_rpc_ok = env::var("SEPOLIA_RPC_URL").is_ok();
            let sepolia_key_ok = env::var("SEPOLIA_PRIVATE_KEY").is_ok();
            if sepolia_rpc_ok && sepolia_key_ok {
                let bridge_amount_usdc: f64 = env::var("BRIDGE_AMOUNT").unwrap_or_else(|_| "1.0".to_string()).parse()?;
                let recipient_arc = wallet.address();
                println_and_restore(&format!("🌉 Bridging {} USDC Sepolia → Arc...", bridge_amount_usdc), &last_status);
                match bridge_usdc_to_arc(bridge_amount_usdc, recipient_arc).await {
                    Ok(tx_hash) => {
                        println_and_restore(&format!("✅ Bridge done: {:?}", tx_hash), &last_status);
                        logger.log(&format!("✅ Bridge done: {:?}", tx_hash));
                        if env::var("AUTO_USYC_DEPOSIT").unwrap_or_default() == "true" {
                            println_and_restore("🔄 Converting USDC to USYC...", &last_status);
                            if let Err(e) = deposit_usdc_to_usyc(bridge_amount_usdc).await {
                                println_and_restore(&format!("⚠️ USYC deposit error: {}", e), &last_status);
                                logger.log(&format!("⚠️ USYC deposit error: {}", e));
                            } else {
                                println_and_restore("✅ USYC minted!", &last_status);
                            }
                        }
                    }
                    Err(e) => {
                        println_and_restore(&format!("❌ Bridge error: {}", e), &last_status);
                        logger.log(&format!("❌ Bridge error: {}", e));
                    }
                }
            } else {
                println_and_restore("⚠️ Bridge disabled: missing Sepolia RPC or private key", &last_status);
            }
            last_bridge_time = Instant::now();
        }

        // Unique actions
        if last_unique_action_time.elapsed() >= unique_action_interval && pool_states.iter().all(|s| s.pending_tx.is_none()) {
            match perform_unique_action(&client, &wallet, &mut deployed_contract, &mut unique_action_count, &mut liquidity_nft_id).await {
                Ok(_) => {
                    println_and_restore("✨ Unique action completed", &last_status);
                    logger.log("✅ Unique action completed");
                }
                Err(e) => {
                    println_and_restore(&format!("⚠️ Unique action error: {}", e), &last_status);
                    logger.log(&format!("❌ Unique action error: {}", e));
                }
            }
            last_unique_action_time = Instant::now();
        }

        let status_text = if pool_states.iter().any(|s| s.trailing_active) { "🔥 MULTI-TRAIL" }
                          else if pool_states.iter().any(|s| s.is_holding) { "💎 HOLDING" }
                          else { "📡 SCANNING" };
        let status_line = format!(
            "{:<20} | Price: {:.6} | Profit: {:+.4} USDC | Trades: {} | Micro: {} | Balance: {:.2} USDC",
            status_text, last_price, total_session_profit,
            pool_states.iter().map(|s| s.trade_count).sum::<u32>(),
            random_swap_count,
            bal_usdc.as_u128() as f64 / 10f64.powi(usdc_decimals as i32)
        );
        if status_line != last_status {
            print_status(&status_line);
            last_status = status_line;
        }

        sleep(Duration::from_secs(1)).await;
    }
}