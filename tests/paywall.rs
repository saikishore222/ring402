mod common;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
    response::Response,
    routing::get,
    Router,
};
use common::{keypair, pay, TOKEN};
use ring402::{
    server::{Ledger, LedgerError, Paywall},
    wire::{
        self, PaymentPayload, PaymentRequired, Proof, Requirements, Session, Settlement,
        PAYMENT_REQUIRED, PAYMENT_RESPONSE, PAYMENT_SESSION, PAYMENT_SIGNATURE, SCHEME,
        SOLANA_DEVNET, X402_VERSION,
    },
    Quote,
};
use solana_signature::Signature;
use spr_keypair::ShieldedKeypair;
use spr_transaction::{Mint, ShieldedTransaction};
use tower::ServiceExt;

const CHEAP: u64 = 1_000;
const PREMIUM: u64 = 5_000;
const IN_TOKEN: u64 = 2_000;

#[derive(Clone, Default)]
struct Chain(Arc<Mutex<HashMap<Signature, ShieldedTransaction>>>);

impl Ledger for Chain {
    fn transactions(&self, signature: Signature) -> Result<Vec<ShieldedTransaction>, LedgerError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(&signature)
            .cloned()
            .into_iter()
            .collect())
    }
}

struct Shop {
    app: Router,
    chain: Chain,
    agent: ShieldedKeypair,
}

impl Shop {
    fn open() -> Self {
        let merchant = keypair(2);
        let chain = Chain::default();
        let paywall = Paywall::new(
            chain.clone(),
            SOLANA_DEVNET,
            merchant.shielded_address().unwrap(),
            merchant.viewing_key.clone(),
        );
        let app = Router::new()
            .route(
                "/cheap",
                get(|| async { "cheap" }).layer(paywall.price(Mint::SOL, CHEAP)),
            )
            .route(
                "/token",
                get(|| async { "token" }).layer(paywall.price(TOKEN, IN_TOKEN)),
            )
            .route(
                "/broken",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR.into_response() })
                    .layer(paywall.price(Mint::SOL, CHEAP)),
            )
            .route(
                "/premium",
                get(|| async { "premium" }).layer(paywall.price(Mint::SOL, PREMIUM)),
            );
        Self {
            app,
            chain,
            agent: keypair(1),
        }
    }

    async fn get(&self, path: &str, payment: Option<&PaymentPayload>) -> Response {
        self.call(path, payment, None).await
    }

    async fn call(
        &self,
        path: &str,
        payment: Option<&PaymentPayload>,
        session: Option<&str>,
    ) -> Response {
        let mut request = Request::get(path);
        if let Some(payment) = payment {
            request = request.header(PAYMENT_SIGNATURE, wire::encode(payment));
        }
        if let Some(token) = session {
            request = request.header(PAYMENT_SESSION, token);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn open_session(&self, credit: u64) -> Session {
        let offer = self.offer("/cheap").await;
        let payment = self.pay_with(&offer, CHEAP + credit, 20);
        session(&self.get("/cheap", Some(&payment)).await).unwrap()
    }

    async fn offer(&self, path: &str) -> Requirements {
        let response = self.get(path, None).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let required: PaymentRequired = header(&response, PAYMENT_REQUIRED);
        required.accepts.into_iter().next().unwrap()
    }

    fn pay(&self, offer: &Requirements, seed: u8) -> PaymentPayload {
        let amount = offer.amount.parse().unwrap();
        self.pay_with(offer, amount, seed)
    }

    fn pay_with(&self, offer: &Requirements, amount: u64, seed: u8) -> PaymentPayload {
        let asset = if offer.asset().unwrap() == TOKEN.asset {
            TOKEN
        } else {
            Mint::SOL
        };
        let quote = Quote {
            amount,
            ..offer.quote(asset).unwrap()
        };
        let (transaction, _) = pay(&self.agent, &quote, 10_000);
        let signature = Signature::from([seed; 64]);
        self.chain.0.lock().unwrap().insert(signature, transaction);
        PaymentPayload {
            x402_version: X402_VERSION,
            resource: None,
            accepted: offer.clone(),
            payload: Proof {
                signature: signature.to_string(),
            },
        }
    }
}

fn header<T: serde::de::DeserializeOwned>(response: &Response, name: &str) -> T {
    wire::decode(response.headers().get(name).unwrap().to_str().unwrap()).unwrap()
}

fn session(response: &Response) -> Option<Session> {
    header::<Settlement>(response, PAYMENT_RESPONSE)
        .extensions
        .and_then(|extensions| extensions.session)
}

async fn text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn an_unpaid_request_gets_a_confidential_offer() {
    let shop = Shop::open();

    let offer = shop.offer("/premium").await;

    assert_eq!(offer.scheme, SCHEME);
    assert_eq!(offer.network, SOLANA_DEVNET);
    assert_eq!(offer.amount, PREMIUM.to_string());
}

#[tokio::test]
async fn a_paid_request_reaches_the_handler() {
    let shop = Shop::open();
    let offer = shop.offer("/premium").await;
    let payment = shop.pay(&offer, 9);

    let response = shop.get("/premium", Some(&payment)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let settlement: Settlement = header(&response, PAYMENT_RESPONSE);
    assert!(settlement.success);
    assert_eq!(settlement.transaction, payment.payload.signature);
    assert_eq!(text(response).await, "premium");
}

#[tokio::test]
async fn a_payment_cannot_be_replayed() {
    let shop = Shop::open();
    let offer = shop.offer("/premium").await;
    let payment = shop.pay(&offer, 9);
    shop.get("/premium", Some(&payment)).await;

    let replay = shop.get("/premium", Some(&payment)).await;

    assert_eq!(replay.status(), StatusCode::PAYMENT_REQUIRED);
    let fresh: PaymentRequired = header(&replay, PAYMENT_REQUIRED);
    assert_ne!(fresh.accepts[0].extra.quote, offer.extra.quote);
}

#[tokio::test]
async fn a_cheap_quote_does_not_open_a_premium_route() {
    let shop = Shop::open();
    let offer = shop.offer("/cheap").await;
    let payment = shop.pay(&offer, 9);

    let response = shop.get("/premium", Some(&payment)).await;

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        shop.get("/cheap", Some(&payment)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_unknown_transaction_is_refused_and_the_quote_survives() {
    let shop = Shop::open();
    let offer = shop.offer("/premium").await;
    let payment = shop.pay(&offer, 9);
    let bogus = PaymentPayload {
        payload: Proof {
            signature: Signature::from([8; 64]).to_string(),
        },
        ..payment.clone()
    };

    let refused = shop.get("/premium", Some(&bogus)).await;

    assert_eq!(refused.status(), StatusCode::PAYMENT_REQUIRED);
    let settlement: Settlement = header(&refused, PAYMENT_RESPONSE);
    assert!(!settlement.success);
    assert_eq!(
        shop.get("/premium", Some(&payment)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_underpayment_is_refused() {
    let shop = Shop::open();
    let offer = shop.offer("/premium").await;
    let short = Requirements {
        amount: (PREMIUM - 1).to_string(),
        ..offer.clone()
    };
    let payment = PaymentPayload {
        accepted: offer,
        ..shop.pay(&short, 9)
    };

    let response = shop.get("/premium", Some(&payment)).await;

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let settlement: Settlement = header(&response, PAYMENT_RESPONSE);
    assert_eq!(
        settlement.error_reason.as_deref(),
        Some("invalid_confidential_svm_payload_amount_mismatch")
    );
}

#[tokio::test]
async fn a_malformed_header_is_a_bad_request() {
    let shop = Shop::open();
    let request = Request::get("/premium")
        .header(PAYMENT_SIGNATURE, "not base64 json")
        .body(Body::empty())
        .unwrap();

    let response = shop.app.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_exact_payment_opens_no_session() {
    let shop = Shop::open();
    let offer = shop.offer("/cheap").await;
    let payment = shop.pay(&offer, 9);

    let response = shop.get("/cheap", Some(&payment)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(session(&response), None);
}

#[tokio::test]
async fn a_surplus_becomes_credit_that_pays_for_later_calls() {
    let shop = Shop::open();
    let offer = shop.offer("/cheap").await;
    let payment = shop.pay_with(&offer, CHEAP * 3, 9);

    let first = shop.get("/cheap", Some(&payment)).await;
    let opened = session(&first).unwrap();
    let second = shop.call("/cheap", None, Some(&opened.token)).await;
    let third = shop.call("/cheap", None, Some(&opened.token)).await;
    let fourth = shop.call("/cheap", None, Some(&opened.token)).await;

    assert_eq!(opened.balance, (CHEAP * 2).to_string());
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        header::<Settlement>(&second, PAYMENT_RESPONSE).transaction,
        ""
    );
    assert_eq!(session(&second).unwrap().balance, CHEAP.to_string());
    assert_eq!(session(&third).unwrap().balance, "0");
    assert_eq!(fourth.status(), StatusCode::PAYMENT_REQUIRED);
}

#[tokio::test]
async fn credit_too_small_for_a_route_is_left_untouched() {
    let shop = Shop::open();
    let opened = shop.open_session(CHEAP * 2).await;

    let premium = shop.call("/premium", None, Some(&opened.token)).await;
    let cheap = shop.call("/cheap", None, Some(&opened.token)).await;

    assert_eq!(premium.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(session(&cheap).unwrap().balance, CHEAP.to_string());
}

#[tokio::test]
async fn an_unknown_session_gets_an_offer() {
    let shop = Shop::open();

    let response = shop.call("/cheap", None, Some("not-a-session")).await;

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let required: PaymentRequired = header(&response, PAYMENT_REQUIRED);
    assert!(required.accepts[0].extra.sessions);
}

#[tokio::test]
async fn a_top_up_adds_to_the_same_session() {
    let shop = Shop::open();
    let opened = shop.open_session(CHEAP).await;
    let offer = shop.offer("/cheap").await;
    let payment = shop.pay_with(&offer, CHEAP * 3, 9);

    let response = shop
        .call("/cheap", Some(&payment), Some(&opened.token))
        .await;

    let topped_up = session(&response).unwrap();
    assert_eq!(topped_up.token, opened.token);
    assert_eq!(topped_up.balance, (CHEAP * 3).to_string());
}

#[tokio::test]
async fn a_failed_handler_is_not_charged_to_the_session() {
    let shop = Shop::open();
    let opened = shop.open_session(CHEAP * 2).await;

    let response = shop.call("/broken", None, Some(&opened.token)).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(session(&response).unwrap().balance, opened.balance);
}

#[tokio::test]
async fn a_failed_handler_turns_an_onchain_payment_into_credit() {
    let shop = Shop::open();
    let offer = shop.offer("/broken").await;
    let payment = shop.pay(&offer, 9);

    let failed = shop.get("/broken", Some(&payment)).await;
    let credited = session(&failed).unwrap();
    let retry = shop.call("/cheap", None, Some(&credited.token)).await;

    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(credited.balance, CHEAP.to_string());
    assert_eq!(retry.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_token_route_is_paid_in_that_token() {
    let shop = Shop::open();
    let offer = shop.offer("/token").await;
    let payment = shop.pay(&offer, 9);

    let response = shop.get("/token", Some(&payment)).await;

    assert_eq!(offer.asset, TOKEN.asset.to_string());
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn sol_credit_does_not_pay_for_a_token_route() {
    let shop = Shop::open();
    let opened = shop.open_session(IN_TOKEN * 2).await;

    let response = shop.call("/token", None, Some(&opened.token)).await;

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
}

#[tokio::test]
async fn one_session_holds_credit_in_each_token() {
    let shop = Shop::open();
    let opened = shop.open_session(CHEAP).await;
    let offer = shop.offer("/token").await;
    let payment = shop.pay_with(&offer, IN_TOKEN * 2, 9);

    let topped_up = shop
        .call("/token", Some(&payment), Some(&opened.token))
        .await;
    let in_token = shop.call("/token", None, Some(&opened.token)).await;
    let in_sol = shop.call("/cheap", None, Some(&opened.token)).await;

    assert_eq!(session(&topped_up).unwrap().token, opened.token);
    assert_eq!(session(&in_token).unwrap().balance, "0");
    assert_eq!(session(&in_sol).unwrap().balance, "0");
}
