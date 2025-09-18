use anyhow::{Context, Result};
use dotenvy::dotenv;
use futures::StreamExt;
use std::{collections::HashMap, env};

use solana_pubkey::Pubkey;

// WS pubsub
use solana_pubsub_client::nonblocking::pubsub_client::PubsubClient;
// Типы для подписок/фильтров и UiAccount
use solana_account_decoder::{UiAccount, UiAccountEncoding};
use solana_rpc_client_api::{
    config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};

// HTTP RPC (для decimals)
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

// SPL token
use spl_token::{self, state::Account as SplAccount};

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let ws_url = env::var("SOLANA_WS_URL").context("SOLANA_WS_URL missing")?;
    let http_url = env::var("SOLANA_HTTP_URL").context("SOLANA_HTTP_URL missing")?;
    let business_wallet = env::var("BUSINESS_WALLET").context("BUSINESS_WALLET missing")?;
    let business_pubkey: Pubkey = business_wallet.parse().context("invalid BUSINESS_WALLET")?;

    // Запускаем одновременно 2 «слушателя»: SOL и SPL
    tokio::try_join!(
        watch_sol(&ws_url, business_pubkey),
        watch_spl_tokens(&ws_url, &http_url, business_pubkey),
    )?;

    Ok(())
}

/* ------------------------- SOL ------------------------- */

async fn watch_sol(ws_url: &str, business: Pubkey) -> Result<()> {
    let client = PubsubClient::new(ws_url).await?;
    let (mut stream, _unsub) = client.account_subscribe(&business, None).await?;
    println!("🔊 [SOL] listening on {business}");

    let mut last: Option<u64> = None;
    while let Some(resp) = stream.next().await {
        let lamports = resp.value.lamports;
        if let Some(prev) = last {
            if lamports != prev {
                let delta = lamports as i64 - prev as i64;
                println!("💰 [SOL] lamports {lamports} (Δ {delta:+})");
            }
        } else {
            println!("🔎 [SOL] initial lamports: {lamports}");
        }
        last = Some(lamports);
    }
    Ok(())
}

/* ------------------------- SPL ------------------------- */

async fn watch_spl_tokens(ws_url: &str, http_url: &str, owner: Pubkey) -> Result<()> {
    // Подключаем PubSub WS и HTTP RPC
    let ps = PubsubClient::new(ws_url).await?;
    let rpc = RpcClient::new(http_url.to_string());

    // Фильтры для programSubscribe по классическому SPL Token Program:
    // - dataSize = 165 (стандартный токен-аккаунт без расширений)
    // - memcmp(offset=32, bytes=owner) — владелец хранится по смещению 32
    let filters = vec![
        RpcFilterType::DataSize(165),
        RpcFilterType::Memcmp(Memcmp {
            offset: 32,
            bytes: MemcmpEncodedBytes::Bytes(owner.to_bytes().to_vec()),
            encoding: None,
        }),
    ];

    let account_cfg = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        ..Default::default()
    };
    let prog_cfg = RpcProgramAccountsConfig {
        filters: Some(filters),
        account_config: account_cfg,
        with_context: Some(true),
    };

    let token_program = spl_token::ID; // Tokenkeg... (классический SPL)
    let (mut stream, _unsub) = ps.program_subscribe(&token_program, Some(prog_cfg)).await?;
    println!("🔊 [SPL] listening token accounts for owner {owner}");

    // Кеш предыдущих amount по каждому Token Account → чтобы находить Δ>0
    let mut last_amounts: HashMap<Pubkey, u64> = HashMap::new();
    // Кеш decimals по mint
    let mut decimals_cache: HashMap<Pubkey, u8> = HashMap::new();

    while let Some(msg) = stream.next().await {
        let keyed = msg.value; // содержит pubkey (token account) и account (UiAccount)
        let ta_pubkey: Pubkey = keyed
            .pubkey
            .parse()
            .with_context(|| format!("bad token account pubkey: {}", keyed.pubkey))?;

        // Достаём сырые байты аккаунта
        let raw_b64 = match &keyed.account {
            UiAccount::Binary(data_b64, _enc) => data_b64,
            UiAccount::LegacyBinary(data_b64) => data_b64,
            UiAccount::Json(_) => {
                // сюда почти не попадаем, но игнорируем json-представление
                continue;
            }
        };
        let raw = base64::decode(raw_b64).context("base64 decode token account")?;

        // Парсим layout SPL-аккаунта
        let spl = SplAccount::unpack_from_slice(&raw)
            .map_err(|e| anyhow::anyhow!("unpack spl Account: {e}"))?;

        // Вычисляем дельту
        let prev = *last_amounts.get(&ta_pubkey).unwrap_or(&0);
        let cur = spl.amount;
        if cur > prev {
            let delta = cur - prev;

            // Узнаём decimals для mint (кешируем)
            let m = spl.mint;
            let decimals = match decimals_cache.get(&m) {
                Some(&d) => d,
                None => {
                    let d = fetch_mint_decimals(&rpc, &m).await
                        .with_context(|| format!("get decimals for mint {m}"))?;
                    decimals_cache.insert(m, d);
                    d
                }
            };

            let delta_ui = format_ui_amount(delta, decimals);
            println!(
                "💸 [SPL] +{} (raw {}) to TA={} mint={} (decimals={})",
                delta_ui, delta, ta_pubkey, m, decimals
            );
        }

        // обновим last
        last_amounts.insert(ta_pubkey, cur);
    }

    Ok(())
}

// Получаем decimals через HTTP RPC `getTokenSupply` (возвращает RpcTokenAmount с полем decimals)
async fn fetch_mint_decimals(rpc: &RpcClient, mint: &Pubkey) -> Result<u8> {
    use solana_rpc_client_api::response::RpcTokenAmount;
    let supply: RpcTokenAmount = rpc
        .get_token_supply(mint)
        .await
        .with_context(|| format!("get_token_supply({mint})"))?;
    Ok(supply.decimals)
}

// Форматируем u64 по decimals -> человекочитаемая строка
fn format_ui_amount(amount: u64, decimals: u8) -> String {
    if decimals == 0 {
        return amount.to_string();
    }
    let d = decimals as u32;
    let scale = 10u128.pow(d);
    let whole = (amount as u128) / scale;
    let frac = (amount as u128) % scale;
    // обрезаем хвостовые нули
    let mut frac_s = format!("{:0width$}", frac, width = d as usize);
    while frac_s.ends_with('0') {
        frac_s.pop();
    }
    if frac_s.is_empty() {
        format!("{whole}")
    } else {
        format!("{whole}.{frac_s}")
    }
}
