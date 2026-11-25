use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, PoisonError,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ring402::{
    client::{Agent, Paid, Receipt},
    helius::Helius,
    server::Paywall,
    usdc,
    wire::{
        self, PAYMENT_REQUIRED, PAYMENT_RESPONSE, PAYMENT_SESSION, PAYMENT_SIGNATURE, SOLANA_DEVNET,
    },
    Opener, Payment,
};
use serde::Serialize;
use serde_json::{json, Value};
use solana_keypair::{read_keypair_file, Keypair};
use solana_signature::Signature;
use spr_client::Rpc;
use spr_keypair::{ShieldedAddress, ShieldedKeypair, ViewingKey};
use spr_transaction::{Address, Mint, ShieldedTransaction};
use tokio::{
    net::TcpListener,
    runtime::Runtime,
    signal::unix::{signal as on_signal, SignalKind},
    sync::Mutex,
};

const PREPAY_UNITS: u64 = 6;
const HOUR: Duration = Duration::from_secs(3600);
const DECIMALS_OFFSET: usize = 44;

const ROUTE: &str = "/forecast";

struct Demo {
    agent: Agent,
    chain: Helius,
    rpc_url: String,
    http: reqwest::Client,
    payer: ShieldedAddress,
    merchant: ShieldedAddress,
    merchant_key: ViewingKey,
    token: Token,
    unit: u64,
    local: String,
    busy: Mutex<()>,
    trace: std::sync::Mutex<Vec<Exchange>>,
    latest: std::sync::Mutex<Option<Shown>>,
    credit_left: AtomicU64,
    payments: Limiter,
    calls: Limiter,
    balances: Limiter,
    requests: Limiter,
    visitor_calls: Visitors,
    visitor_requests: Visitors,
}

struct Limiter {
    most: usize,
    seen: std::sync::Mutex<VecDeque<Instant>>,
}

struct Visitors {
    most: usize,
    seen: std::sync::Mutex<HashMap<String, VecDeque<Instant>>>,
}

#[derive(Serialize)]
struct Exchange {
    method: String,
    path: String,
    status: u16,
    sent: Vec<Header>,
    received: Vec<Header>,
}

#[derive(Serialize)]
struct Header {
    name: &'static str,
    value: String,
    decoded: Option<Value>,
}

struct Token {
    mint: Mint,
    symbol: String,
    decimals: u32,
}

#[derive(Serialize)]
struct Call {
    route: String,
    price: String,
    millis: u64,
    status: u16,
    body: String,
    credit: Option<String>,
    payment: Option<Shown>,
    credit_from: Option<Shown>,
    exchanges: Vec<Exchange>,
}

#[derive(Clone, Serialize)]
struct Shown {
    signature: String,
    public: Public,
    merchant: Option<Reading>,
    auditor: Option<Reading>,
    stranger: Option<Reading>,
}

#[derive(Clone, Serialize)]
struct Public {
    signer: String,
    bytes: usize,
    ciphertext: String,
}

#[derive(Clone, Serialize)]
struct Reading {
    amount: String,
    token: String,
    mint: String,
    quote: String,
}

type Failure = (StatusCode, String);

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY")?;
    let payer = payer()?;
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = match std::env::var("PORT") {
        Ok(port) => port.parse().context("PORT is not a port number")?,
        Err(_) => 4020,
    };
    let payments_per_hour = match std::env::var("PAYMENTS_PER_HOUR") {
        Ok(payments) => payments
            .parse()
            .context("PAYMENTS_PER_HOUR is not a number")?,
        Err(_) => 20,
    };

    let chain = Helius::devnet(&api_key);
    let token = Token::from_env(&chain)?;
    let asset = token.mint.asset;
    let unit = if token.mint == Mint::SOL {
        3_000_000
    } else {
        10_000
    };
    let prepay = unit * PREPAY_UNITS;

    let merchant = ShieldedKeypair::from_keypair(&Keypair::new())?;
    let pay_to = merchant.shielded_address()?;
    let paywall = Paywall::new(
        Helius::devnet(&api_key),
        SOLANA_DEVNET,
        pay_to,
        merchant.viewing_key.clone(),
    );
    let agent = Agent::new(Helius::devnet(&api_key), &payer)?
        .spend_limit(asset, unit)
        .prepay(asset, prepay);
    let forecast = get(forecast).layer(paywall.price(token.mint, unit));

    Runtime::new()?.block_on(async move {
        let demo = Arc::new(Demo {
            payer: agent.address()?,
            agent,
            chain,
            rpc_url: format!("https://devnet.helius-rpc.com/?api-key={api_key}"),
            http: reqwest::Client::new(),
            merchant: pay_to,
            merchant_key: merchant.viewing_key.clone(),
            token,
            unit,
            local: format!("http://127.0.0.1:{port}"),
            busy: Mutex::new(()),
            trace: Default::default(),
            latest: Default::default(),
            credit_left: AtomicU64::new(0),
            payments: Limiter::new(payments_per_hour),
            calls: Limiter::new(1_000),
            balances: Limiter::new(1_000),
            requests: Limiter::new(3_000),
            visitor_calls: Visitors::new(20),
            visitor_requests: Visitors::new(200),
        });
        let apis = Router::new()
            .route(ROUTE, forecast)
            .layer(from_fn_with_state(demo.clone(), trace));
        let app = Router::new()
            .route("/", get(|| async { Html(include_str!("demo.html")) }))
            .route("/demo/state", get(state))
            .route("/demo/balance", get(balance))
            .route("/demo/call", post(call))
            .with_state(demo.clone())
            .merge(apis);
        let listener = TcpListener::bind((host.as_str(), port)).await?;
        println!("open http://{host}:{port}");
        tokio::spawn(async move {
            let _one_at_a_time = demo.busy.lock().await;
            if let Err(error) = demo.top_up().await {
                eprintln!("funding the agent: {}", redacted(&error.to_string()));
            }
        });
        axum::serve(listener, app)
            .with_graceful_shutdown(stopped())
            .await?;
        Ok(())
    })
}

async fn forecast() -> Json<Value> {
    Json(json!({ "city": "Bengaluru", "tomorrow": "light rain", "high_c": 27 }))
}

async fn state(State(demo): State<Arc<Demo>>) -> Result<Json<Value>, Failure> {
    Ok(Json(json!({
        "network": "devnet",
        "agent": demo.payer.solana_address().map_err(failure)?.to_string(),
        "merchant": demo.merchant.solana_address().map_err(failure)?.to_string(),
        "prepay": demo.token.format(demo.unit * PREPAY_UNITS),
        "route": ROUTE,
        "price": demo.token.format(demo.unit),
        "latest": *demo.latest.lock().unwrap_or_else(PoisonError::into_inner),
    })))
}

async fn balance(
    State(demo): State<Arc<Demo>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Failure> {
    demo.visitor_requests.check(&headers)?;
    demo.balances.check()?;
    let units = demo
        .agent
        .balance(demo.token.mint.asset)
        .await
        .map_err(failure)?;
    Ok(Json(json!({ "balance": demo.token.format(units) })))
}

async fn call(State(demo): State<Arc<Demo>>, headers: HeaderMap) -> Result<Json<Call>, Failure> {
    demo.visitor_calls.check(&headers)?;
    let _one_at_a_time = demo.busy.lock().await;
    demo.calls.check()?;
    if demo.credit_left.load(Ordering::Relaxed) < demo.unit {
        demo.payments.check()?;
        demo.top_up().await.map_err(failure)?;
    }
    demo.trace
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    let started = Instant::now();
    let Paid {
        response,
        receipt,
        credit,
    } = demo
        .agent
        .get(&format!("{}{ROUTE}", demo.local))
        .await
        .map_err(failure)?;
    demo.credit_left
        .store(credit.unwrap_or_default(), Ordering::Relaxed);
    let status = response.status().as_u16();
    let body = response.text().await.map_err(failure)?;
    let millis = started.elapsed().as_millis() as u64;
    let payment = match receipt {
        Some(receipt) => Some(demo.show(receipt).await.map_err(failure)?),
        None => None,
    };
    let credit_from = {
        let mut latest = demo.latest.lock().unwrap_or_else(PoisonError::into_inner);
        match &payment {
            Some(paid) => {
                *latest = Some(paid.clone());
                None
            }
            None => latest.clone(),
        }
    };
    let exchanges = std::mem::take(&mut *demo.trace.lock().unwrap_or_else(PoisonError::into_inner));
    Ok(Json(Call {
        route: ROUTE.to_string(),
        price: demo.token.format(demo.unit),
        millis,
        status,
        body,
        credit: credit.map(|credit| demo.token.format(credit)),
        payment,
        credit_from,
        exchanges,
    }))
}

async fn trace(State(demo): State<Arc<Demo>>, request: Request, next: Next) -> Response {
    if let Err(limited) = demo
        .visitor_requests
        .check(request.headers())
        .and_then(|()| demo.requests.check())
    {
        return limited.into_response();
    }
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let sent = x402_headers(request.headers());
    let response = next.run(request).await;
    let exchange = Exchange {
        method,
        path,
        status: response.status().as_u16(),
        sent,
        received: x402_headers(response.headers()),
    };
    demo.trace
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(exchange);
    response
}

fn x402_headers(headers: &HeaderMap) -> Vec<Header> {
    [
        PAYMENT_REQUIRED,
        PAYMENT_SIGNATURE,
        PAYMENT_RESPONSE,
        PAYMENT_SESSION,
    ]
    .into_iter()
    .filter_map(|name| {
        let value = headers.get(name)?.to_str().ok()?.to_string();
        let decoded = wire::decode::<Value>(&value).ok();
        Some(Header {
            name,
            value,
            decoded,
        })
    })
    .collect()
}

fn failure(error: impl std::fmt::Display) -> Failure {
    (StatusCode::BAD_GATEWAY, redacted(&error.to_string()))
}

fn redacted(text: &str) -> String {
    const KEY: &str = "api-key=";
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        let (head, tail) = rest.split_at(at + KEY.len());
        kept.push_str(head);
        kept.push_str("<redacted>");
        rest = tail.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-');
    }
    kept.push_str(rest);
    kept
}

fn payer() -> Result<Keypair> {
    if let Ok(secret) = std::env::var("PAYER_SECRET") {
        let bytes: Vec<u8> = serde_json::from_str(&secret)
            .context("PAYER_SECRET must be the JSON array from a keypair file")?;
        return Keypair::try_from(bytes.as_slice())
            .map_err(|_| anyhow!("PAYER_SECRET is not a keypair"));
    }
    let path = std::env::var("PAYER_KEYPAIR").unwrap_or_else(|_| "~/.config/solana/id.json".into());
    read_keypair_file(shellexpand::tilde(&path).as_ref()).map_err(|e| anyhow!("load {path}: {e}"))
}

impl Limiter {
    fn new(most: usize) -> Self {
        Self {
            most,
            seen: Default::default(),
        }
    }

    fn check(&self) -> Result<(), Failure> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if !admit(&mut seen, self.most, Instant::now()) {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "the demo has hit its hourly limit, try again later".into(),
            ));
        }
        Ok(())
    }
}

impl Visitors {
    fn new(most: usize) -> Self {
        Self {
            most,
            seen: Default::default(),
        }
    }

    fn check(&self, headers: &HeaderMap) -> Result<(), Failure> {
        let Some(visitor) = visitor(headers) else {
            return Ok(());
        };
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, times| {
            times
                .back()
                .is_some_and(|at| now.duration_since(*at) <= HOUR)
        });
        if !admit(seen.entry(visitor).or_default(), self.most, now) {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "you've sent a lot of requests, try again in a while".into(),
            ));
        }
        Ok(())
    }
}

fn admit(seen: &mut VecDeque<Instant>, most: usize, now: Instant) -> bool {
    while seen
        .front()
        .is_some_and(|at| now.duration_since(*at) > HOUR)
    {
        seen.pop_front();
    }
    if seen.len() >= most {
        return false;
    }
    seen.push_back(now);
    true
}

fn visitor(headers: &HeaderMap) -> Option<String> {
    let forwarded = headers.get("x-forwarded-for")?.to_str().ok()?;
    Some(forwarded.rsplit(',').next()?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::{redacted, Visitors};

    #[test]
    fn each_visitor_gets_their_own_limit() {
        let from = |forwarded: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", forwarded.parse().unwrap());
            headers
        };
        let visitors = Visitors::new(2);

        assert!(visitors.check(&from("1.1.1.1")).is_ok());
        assert!(visitors.check(&from("9.9.9.9, 1.1.1.1")).is_ok());
        assert!(visitors.check(&from("1.1.1.1")).is_err());
        assert!(visitors.check(&from("2.2.2.2")).is_ok());
        assert!(visitors.check(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn api_keys_are_cut_out_of_errors() {
        let error = "error sending request for url (https://devnet.helius-rpc.com/v1/spr?api-key=0000aaaa-bbbb-cccc&x=1) and https://h/?api-key=abc";
        assert_eq!(
            redacted(error),
            "error sending request for url (https://devnet.helius-rpc.com/v1/spr?api-key=<redacted>&x=1) and https://h/?api-key=<redacted>"
        );
    }
}

async fn stopped() {
    let mut terminate = on_signal(SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

impl Demo {
    async fn top_up(&self) -> Result<()> {
        let asset = self.token.mint.asset;
        let prepay = self.unit * PREPAY_UNITS;
        if self.agent.balance(asset).await? >= prepay * 2 {
            return Ok(());
        }
        let signature = self.agent.deposit(asset, prepay * 6).await?;
        println!("deposited {} in {signature}", self.token.format(prepay * 6));
        Ok(())
    }

    async fn show(self: &Arc<Self>, receipt: Receipt) -> Result<Shown> {
        let signature = receipt.signature;
        let demo = self.clone();
        let indexed =
            tokio::task::spawn_blocking(move || demo.chain.transactions(signature)).await??;
        let read = |opener: Opener<'_>| {
            let payment = receipt.quote.settle(indexed.first()?, opener).ok()?;
            Some(self.reading(payment))
        };
        Ok(Shown {
            signature: signature.to_string(),
            merchant: read(Opener::Recipient(&self.merchant_key)),
            auditor: read(Opener::Disclosed(&receipt.key)),
            stranger: read(Opener::Recipient(&ViewingKey::new())),
            public: self.public(signature, &indexed).await?,
        })
    }

    fn reading(&self, payment: Payment) -> Reading {
        Reading {
            amount: self.token.format(payment.amount),
            token: self.token.symbol.clone(),
            mint: self.token.mint.asset.to_string(),
            quote: hex::encode(payment.quote_id),
        }
    }

    async fn public(
        &self,
        signature: Signature,
        indexed: &[ShieldedTransaction],
    ) -> Result<Public> {
        let tx = self.transaction(signature).await?;
        let to_merchant = self.merchant.confidential_view_tag()?;
        let output = indexed
            .iter()
            .flat_map(|tx| &tx.output_slots)
            .find(|slot| slot.view_tag == to_merchant)
            .context("no output for the merchant")?;
        Ok(Public {
            signer: tx["transaction"]["message"]["accountKeys"][0]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            bytes: output.payload.len(),
            ciphertext: hex::encode(&output.payload[..output.payload.len().min(48)]),
        })
    }

    async fn transaction(&self, signature: Signature) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTransaction",
            "params": [signature.to_string(), {
                "encoding": "json",
                "commitment": "confirmed",
                "maxSupportedTransactionVersion": 1,
            }],
        });
        for _ in 0..10 {
            let response = self
                .http
                .post(&self.rpc_url)
                .header("content-type", "application/json")
                .body(request.to_string())
                .send()
                .await?
                .text()
                .await?;
            let mut response: Value = serde_json::from_str(&response)?;
            if response["result"].is_object() {
                return Ok(response["result"].take());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(anyhow!("transaction {signature} not found"))
    }
}

impl Token {
    fn from_env(chain: &Helius) -> Result<Self> {
        let Ok(mint) = std::env::var("MINT") else {
            return Ok(Self {
                mint: Mint::SOL,
                symbol: "SOL".into(),
                decimals: 9,
            });
        };
        let address: Address = mint
            .parse()
            .map_err(|_| anyhow!("MINT is not an address"))?;
        let account = chain
            .client()
            .get_account(address)?
            .context("mint not found")?;
        Ok(Self {
            mint: chain.mint(address)?,
            symbol: match address {
                usdc::DEVNET | usdc::MAINNET => "USDC".into(),
                _ => format!("{}…", &mint[..4]),
            },
            decimals: (*account.data.get(DECIMALS_OFFSET).context("not a mint")?).into(),
        })
    }

    fn format(&self, units: u64) -> String {
        let scale = 10u64.pow(self.decimals);
        let fraction = format!("{:0width$}", units % scale, width = self.decimals as usize);
        let fraction = fraction.trim_end_matches('0');
        match fraction {
            "" => format!("{} {}", units / scale, self.symbol),
            _ => format!("{}.{fraction} {}", units / scale, self.symbol),
        }
    }
}
