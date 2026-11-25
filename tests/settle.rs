mod common;

use common::{keypair, pay, quote, TOKEN};
use ring402::{Error, Opener, Payment, Quote};
use spr_transaction::ShieldedTransaction;

fn settle(quote: &Quote, tx: &ShieldedTransaction, opener: Opener<'_>) -> Result<Payment, Error> {
    quote.settle(tx, opener)
}

#[test]
fn merchant_settles_with_only_a_viewing_key() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let quote = quote(&merchant, 3_000);
    let (proof, _) = pay(&agent, &quote, 10_000);

    let view_only = merchant.viewing_key.clone();
    let payment = settle(&quote, &proof, Opener::Recipient(&view_only)).unwrap();

    assert_eq!(payment.quote_id, quote.id);
    assert_eq!(payment.amount, 3_000);
}

#[test]
fn payer_discloses_one_payment_with_its_transaction_key() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let quote = quote(&merchant, 3_000);
    let (proof, tx_key) = pay(&agent, &quote, 10_000);

    let disclosed = settle(&quote, &proof, Opener::Disclosed(&tx_key)).unwrap();
    let received = settle(&quote, &proof, Opener::Recipient(&merchant.viewing_key)).unwrap();

    assert_eq!(disclosed, received);
}

#[test]
fn a_stranger_cannot_open_the_payment() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let stranger = keypair(3);
    let quote = quote(&merchant, 3_000);
    let (proof, _) = pay(&agent, &quote, 10_000);

    let result = settle(&quote, &proof, Opener::Recipient(&stranger.viewing_key));

    assert!(matches!(result, Err(Error::NotPaid)));
}

#[test]
fn a_payment_for_another_quote_is_not_accepted() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let paid = quote(&merchant, 3_000);
    let (proof, _) = pay(&agent, &paid, 10_000);

    let other = Quote {
        id: [0xCD; 16],
        ..paid
    };
    let result = settle(&other, &proof, Opener::Recipient(&merchant.viewing_key));

    assert!(matches!(result, Err(Error::NotPaid)));
}

#[test]
fn underpayment_is_reported_with_both_amounts() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let paid = quote(&merchant, 3_000);
    let (proof, _) = pay(&agent, &paid, 10_000);

    let pricier = Quote {
        amount: 5_000,
        ..paid
    };
    let result = settle(&pricier, &proof, Opener::Recipient(&merchant.viewing_key));

    assert!(matches!(
        result,
        Err(Error::Underpaid {
            paid: 3_000,
            asked: 5_000
        })
    ));
}

#[test]
fn a_tampered_commitment_is_rejected() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let quote = quote(&merchant, 3_000);
    let (mut proof, _) = pay(&agent, &quote, 10_000);

    for slot in &mut proof.output_slots {
        slot.output_context.hash[0] ^= 1;
    }
    let result = settle(&quote, &proof, Opener::Recipient(&merchant.viewing_key));

    assert!(matches!(result, Err(Error::CommitmentMismatch)));
}

#[test]
fn a_token_payment_settles() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let quote = Quote {
        asset: TOKEN,
        ..quote(&merchant, 3_000)
    };
    let (proof, _) = pay(&agent, &quote, 10_000);

    let payment = settle(&quote, &proof, Opener::Recipient(&merchant.viewing_key)).unwrap();

    assert_eq!(payment.asset_id, TOKEN.asset_id);
    assert_eq!(payment.amount, 3_000);
}

#[test]
fn a_token_payment_does_not_settle_a_sol_quote() {
    let agent = keypair(1);
    let merchant = keypair(2);
    let in_sol = quote(&merchant, 3_000);
    let in_token = Quote {
        asset: TOKEN,
        ..in_sol.clone()
    };
    let (proof, _) = pay(&agent, &in_token, 10_000);

    let result = settle(&in_sol, &proof, Opener::Recipient(&merchant.viewing_key));

    assert!(matches!(result, Err(Error::NotPaid)));
}
