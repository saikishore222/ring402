use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    extract::Request,
    http::{header::CONTENT_TYPE, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use solana_signature::Signature;
use spr_keypair::{ShieldedAddress, ViewingKey};
use spr_transaction::{Address, Mint, ShieldedTransaction};
use tower::{Layer, Service};

use crate::{
    helius::Helius,
    wire::{
        self, Extensions, PaymentPayload, PaymentRequired, Requirements, Resource, Session,
        Settlement, PAYMENT_REQUIRED, PAYMENT_RESPONSE, PAYMENT_SESSION, PAYMENT_SIGNATURE,
        X402_VERSION,
    },
    Error, Opener, Payment, Quote,
};

const QUOTE_TTL: Duration = Duration::from_secs(120);

pub type LedgerError = Box<dyn std::error::Error + Send + Sync>;

pub trait Ledger: Send + Sync + 'static {
    fn transactions(&self, signature: Signature) -> Result<Vec<ShieldedTransaction>, LedgerError>;
}

impl Ledger for Helius {
    fn transactions(&self, signature: Signature) -> Result<Vec<ShieldedTransaction>, LedgerError> {
        Ok(Helius::transactions(self, signature)?)
    }
}

#[derive(Clone)]
pub struct Paywall(Arc<Merchant>);

struct Merchant {
    ledger: Arc<dyn Ledger>,
    network: String,
    pay_to: ShieldedAddress,
    viewing_key: ViewingKey,
    issued: Mutex<HashMap<[u8; 16], Issued>>,
    sessions: Mutex<HashMap<String, HashMap<Address, u64>>>,
}

struct Issued {
    quote: Quote,
    path: String,
    expires: Instant,
}

struct Admission {
    settlement: Settlement,
    session: Option<String>,
}

#[derive(Clone)]
pub struct Price {
    paywall: Paywall,
    asset: Mint,
    amount: u64,
}

#[derive(Clone)]
pub struct Priced<S> {
    price: Price,
    inner: S,
}

impl Paywall {
    pub fn new(
        ledger: impl Ledger,
        network: impl Into<String>,
        pay_to: ShieldedAddress,
        viewing_key: ViewingKey,
    ) -> Self {
        Self(Arc::new(Merchant {
            ledger: Arc::new(ledger),
            network: network.into(),
            pay_to,
            viewing_key,
            issued: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }))
    }

    pub fn price(&self, asset: Mint, amount: u64) -> Price {
        Price {
            paywall: self.clone(),
            asset,
            amount,
        }
    }
}

impl Merchant {
    fn issue(&self, asset: Mint, amount: u64, path: &str) -> Quote {
        let quote = Quote {
            id: random(),
            asset,
            amount,
            pay_to: self.pay_to,
        };
        let now = Instant::now();
        let mut issued = locked(&self.issued);
        issued.retain(|_, entry| entry.expires > now);
        issued.insert(
            quote.id,
            Issued {
                quote: quote.clone(),
                path: path.to_string(),
                expires: now + QUOTE_TTL,
            },
        );
        quote
    }

    fn claim(&self, id: &[u8; 16], price: &Price, path: &str) -> Option<Issued> {
        let mut issued = locked(&self.issued);
        let entry = issued.get(id)?;
        let matches = entry.expires > Instant::now()
            && entry.path == path
            && entry.quote.asset == price.asset
            && entry.quote.amount == price.amount;
        matches.then(|| issued.remove(id)).flatten()
    }

    fn restore(&self, entry: Issued) {
        locked(&self.issued).insert(entry.quote.id, entry);
    }

    fn settle(
        &self,
        quote: &Quote,
        transactions: &[ShieldedTransaction],
    ) -> Result<Payment, Error> {
        let mut refusal = Error::NotPaid;
        for tx in transactions {
            match quote.settle(tx, Opener::Recipient(&self.viewing_key)) {
                Ok(payment) => return Ok(payment),
                Err(Error::NotPaid) => {}
                Err(error) => refusal = error,
            }
        }
        Err(refusal)
    }

    fn debit(&self, token: &str, price: &Price) -> Option<Session> {
        let mut sessions = locked(&self.sessions);
        let balance = sessions.get_mut(token)?.get_mut(&price.asset.asset)?;
        *balance = balance.checked_sub(price.amount)?;
        Some(Session {
            token: token.to_string(),
            balance: balance.to_string(),
        })
    }

    fn credit(&self, token: Option<String>, asset: Mint, amount: u64) -> Session {
        let mut sessions = locked(&self.sessions);
        let token = token
            .filter(|token| sessions.contains_key(token))
            .unwrap_or_else(|| hex::encode(random::<32>()));
        let balance = sessions
            .entry(token.clone())
            .or_default()
            .entry(asset.asset)
            .or_default();
        *balance = balance.saturating_add(amount);
        Session {
            token,
            balance: balance.to_string(),
        }
    }

    fn settled(&self, transaction: String, session: Option<Session>) -> Settlement {
        Settlement {
            success: true,
            error_reason: None,
            transaction,
            network: self.network.clone(),
            extensions: session.map(|session| Extensions {
                session: Some(session),
            }),
        }
    }
}

impl Price {
    async fn admit(
        &self,
        payment: Option<HeaderValue>,
        session: Option<String>,
        uri: String,
        path: String,
    ) -> Result<Admission, Response> {
        let merchant = &self.paywall.0;
        let Some(payment) = payment else {
            let Some(token) = session else {
                return Err(self.challenge(&uri, &path, "PAYMENT-SIGNATURE header is required"));
            };
            let Some(charged) = merchant.debit(&token, self) else {
                return Err(self.challenge(&uri, &path, "session is unknown or out of credit"));
            };
            return Ok(Admission {
                settlement: merchant.settled(String::new(), Some(charged)),
                session: Some(token),
            });
        };
        let Some((id, signature)) = parse(&payment) else {
            return Err((
                StatusCode::BAD_REQUEST,
                "malformed PAYMENT-SIGNATURE header",
            )
                .into_response());
        };
        let Some(entry) = merchant.claim(&id, self, &path) else {
            return Err(self.challenge(&uri, &path, "quote is unknown, expired or already used"));
        };

        let ledger = merchant.ledger.clone();
        let found = tokio::task::spawn_blocking(move || ledger.transactions(signature)).await;
        let reason = match found {
            Ok(Ok(transactions)) => match merchant.settle(&entry.quote, &transactions) {
                Ok(paid) => {
                    let surplus = paid.amount - self.amount;
                    let opened = (surplus > 0)
                        .then(|| merchant.credit(session.clone(), self.asset, surplus));
                    return Ok(Admission {
                        session: opened
                            .as_ref()
                            .map(|opened| opened.token.clone())
                            .or(session),
                        settlement: merchant.settled(signature.to_string(), opened),
                    });
                }
                Err(Error::Underpaid { .. }) => "invalid_confidential_svm_payload_amount_mismatch",
                Err(Error::CommitmentMismatch) => "invalid_confidential_svm_payload_commitment",
                Err(_) => "invalid_confidential_svm_payload_not_paid",
            },
            _ => "invalid_transaction_state",
        };
        merchant.restore(entry);
        Err(refused(Settlement {
            success: false,
            error_reason: Some(reason.to_string()),
            transaction: signature.to_string(),
            network: merchant.network.clone(),
            extensions: None,
        }))
    }

    fn refund(&self, session: Option<String>) -> Extensions {
        Extensions {
            session: Some(self.paywall.0.credit(session, self.asset, self.amount)),
        }
    }

    fn challenge(&self, uri: &str, path: &str, error: &str) -> Response {
        let merchant = &self.paywall.0;
        let quote = merchant.issue(self.asset, self.amount, path);
        let required = PaymentRequired {
            x402_version: X402_VERSION,
            error: Some(error.to_string()),
            resource: Resource {
                url: uri.to_string(),
                description: None,
                mime_type: None,
            },
            accepts: vec![Requirements::offer(&quote, &merchant.network, QUOTE_TTL)],
        };
        let body = serde_json::to_string(&required).expect("wire types serialize");
        let mut response = (StatusCode::PAYMENT_REQUIRED, body).into_response();
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(PAYMENT_REQUIRED, encoded(&required));
        response
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes
}

fn parse(header: &HeaderValue) -> Option<([u8; 16], Signature)> {
    let payload: PaymentPayload = wire::decode(header.to_str().ok()?).ok()?;
    let mut id = [0; 16];
    hex::decode_to_slice(&payload.accepted.extra.quote, &mut id).ok()?;
    Some((id, Signature::from_str(&payload.payload.signature).ok()?))
}

fn refused(settlement: Settlement) -> Response {
    let mut response = StatusCode::PAYMENT_REQUIRED.into_response();
    response
        .headers_mut()
        .insert(PAYMENT_RESPONSE, encoded(&settlement));
    response
}

fn encoded<T: serde::Serialize>(value: &T) -> HeaderValue {
    HeaderValue::from_str(&wire::encode(value)).expect("base64 is a valid header value")
}

impl<S> Layer<S> for Price {
    type Service = Priced<S>;

    fn layer(&self, inner: S) -> Priced<S> {
        Priced {
            price: self.clone(),
            inner,
        }
    }
}

impl<S> Service<Request> for Priced<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let price = self.price.clone();
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let headers = request.headers();
        let payment = headers.get(PAYMENT_SIGNATURE).cloned();
        let session = headers
            .get(PAYMENT_SESSION)
            .and_then(|token| token.to_str().ok())
            .map(str::to_string);
        let uri = request.uri().to_string();
        let path = request.uri().path().to_string();

        Box::pin(async move {
            let mut admission = match price.admit(payment, session, uri, path).await {
                Ok(admission) => admission,
                Err(refusal) => return Ok(refusal),
            };
            let mut response = inner.call(request).await?;
            if response.status().is_server_error() {
                admission.settlement.extensions = Some(price.refund(admission.session));
            }
            response
                .headers_mut()
                .insert(PAYMENT_RESPONSE, encoded(&admission.settlement));
            Ok(response)
        })
    }
}
