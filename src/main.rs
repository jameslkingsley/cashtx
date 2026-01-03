#![allow(dead_code)]

use std::{fmt::Display, io::stdin, ops::Div};

use anyhow::Result;
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use clap::Parser;
use regex::Regex;
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use reqwest_middleware::ClientWithMiddleware;
use rust_decimal::{
    Decimal,
    prelude::{FromPrimitive, ToPrimitive},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::http_client;

mod http;

#[derive(Debug, Clone, Parser)]
struct Args {
    #[arg(short, long)]
    since: NaiveDate,

    #[clap(env = "SQUARE_SHIFT_EVENT_DESCRIPTION_EXCLUSIONS_PATTERN")]
    exclusions: String,

    #[clap(env)]
    square_location_id: String,

    #[clap(env)]
    square_app_id: String,

    #[clap(env)]
    square_access_token: String,

    #[clap(env)]
    xero_client_id: String,

    #[clap(env)]
    xero_client_secret: String,

    #[clap(env)]
    xero_tenant_id: String,

    #[clap(env)]
    xero_payment_account_code: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Shift {
    id: String,
    state: ShiftState,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ShiftState {
    Open,
    Closed,
    Ended,
}

#[derive(Debug, Clone, Deserialize)]
struct GetShiftsResponse {
    cash_drawer_shifts: Vec<Shift>,
}

#[derive(Debug, Clone, Deserialize)]
struct ShiftEvent {
    event_type: ShiftEventType,
    event_money: ShiftEventMoney,
    description: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ShiftEventType {
    NoSale,
    CashTenderPayment,
    OtherTenderPayment,
    CashTenderCancelledPayment,
    OtherTenderCancelledPayment,
    CashTenderRefund,
    OtherTenderRefund,
    PaidIn,
    PaidOut,
}

#[derive(Debug, Clone, Deserialize)]
struct ShiftEventMoney {
    amount: f64,
    currency: String,
}

impl ShiftEventMoney {
    fn as_dec(&self) -> f64 {
        let dec = Decimal::from_f64(self.amount.div(100.0)).unwrap();
        dec.to_f64().unwrap()
    }
}

impl Display for ShiftEventMoney {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", Decimal::from_f64(self.amount.div(100.0)).unwrap())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GetShiftEventsResponse {
    cash_drawer_shift_events: Vec<ShiftEvent>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GetInvoicesResponse {
    invoices: Vec<Invoice>,
}

#[derive(Debug, Default, Clone)]
enum InvoiceMatchResult {
    #[default]
    None,
    AlreadyPaid,
    UnpaidSingle(Invoice),
    UnpaidMultiple(Vec<Invoice>),
}

fn fuzzy_matches_contact(event: &ShiftEvent, contact: &Contact) -> bool {
    let Some(desc) = event.description.as_deref() else {
        return false;
    };

    let threshold =
        rapidfuzz::distance::lcs_seq::normalized_similarity(desc.chars(), contact.name.chars());

    threshold >= 0.2
}

impl GetInvoicesResponse {
    fn find_match(&self, event: &ShiftEvent) -> InvoiceMatchResult {
        let event_dec = Decimal::from_f64(event.event_money.amount)
            .unwrap()
            .div(Decimal::from(100))
            .normalize()
            .round_dp(2);

        let matching_by_amount = self
            .invoices
            .iter()
            .filter(|inv| inv.invoice_type == InvoiceType::AccPay)
            .filter(|inv| fuzzy_matches_contact(event, &inv.contact))
            .filter(|inv| {
                let amount_due = Decimal::from_f64(inv.amount_due)
                    .unwrap()
                    .normalize()
                    .round_dp(2);
                let amount_paid = Decimal::from_f64(inv.amount_paid)
                    .unwrap()
                    .normalize()
                    .round_dp(2);
                amount_due == event_dec || amount_paid == event_dec
            })
            .cloned()
            .collect::<Vec<_>>();

        if matching_by_amount.is_empty() {
            return InvoiceMatchResult::None;
        }

        let (paid, unpaid): (Vec<_>, Vec<_>) = matching_by_amount
            .iter()
            .partition(|inv| inv.amount_paid > 0.0);

        if !paid.is_empty() {
            return InvoiceMatchResult::AlreadyPaid;
        }

        let unpaid = unpaid.into_iter().cloned().collect::<Vec<_>>();

        match unpaid.len() {
            0 => InvoiceMatchResult::None,
            1 => InvoiceMatchResult::UnpaidSingle(unpaid[0].clone()),
            _ => InvoiceMatchResult::UnpaidMultiple(unpaid),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Invoice {
    #[serde(rename = "Type")]
    invoice_type: InvoiceType,
    #[serde(rename = "InvoiceID")]
    invoice_id: String,
    invoice_number: String,
    amount_due: f64,
    amount_paid: f64,
    contact: Contact,
    #[serde(rename = "DateString")]
    date: NaiveDateTime,
    #[serde(rename = "DueDateString")]
    due_date: NaiveDateTime,
    status: InvoiceStatus,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
enum InvoiceType {
    AccPay,
    AccRec,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum InvoiceStatus {
    Draft,
    Submitted,
    Authorised,
    Paid,
    Deleted,
    Voided,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Contact {
    #[serde(rename = "ContactID")]
    contact_id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct PaymentRequestObject {
    invoice: PaymentRequestObjectInvoice,
    account: PaymentRequestObjectAccount,
    date: NaiveDate,
    amount: f64,
    reference: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PaymentRequestObjectInvoice {
    #[serde(rename = "InvoiceID")]
    invoice_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PaymentRequestObjectAccount {
    #[serde(rename = "Code")]
    code: String,
}

#[tokio::main]
pub async fn main() -> Result<()> {
    dotenvy::dotenv()?;
    let args = Args::parse();

    let square = square_client(&args);
    let xero = xero_client(&args).await;

    println!();

    if !args.exclusions.is_empty() {
        println!("   Excluding shift events matching: {}", args.exclusions);
    }

    let invoices: GetInvoicesResponse = match xero
        .get("https://api.xero.com/api.xro/2.0/Invoices")
        .query(&[("Statuses", "AUTHORISED,PAID"), ("pageSize", "1000")])
        .send()
        .await?
        .error_for_status()
    {
        Ok(res) => res.json().await?,
        Err(err) => {
            eprintln!("   Failed to get invoices: {err}");
            return Ok(());
        }
    };

    println!("   Retrieved {} invoices", invoices.invoices.len());
    if invoices.invoices.len() == 1000 {
        println!("   Warning: invoice count matches page size; run again after this");
    }

    let shifts: GetShiftsResponse = match square
        .get("https://connect.squareup.com/v2/cash-drawers/shifts")
        .query(&[
            ("location_id", &args.square_location_id),
            (
                "begin_time",
                &args.since.format("%Y-%m-%dT00:00:00.0000").to_string(),
            ),
        ])
        .send()
        .await?
        .error_for_status()
    {
        Ok(res) => res.json().await?,
        Err(err) => {
            eprintln!("Failed to get shifts: {err}");
            return Ok(());
        }
    };

    let mut unmatched = Vec::new();
    let mut matched: Vec<(Shift, ShiftEvent, Invoice)> = Vec::new();
    let mut already_paid = Vec::new();

    println!(
        "   Processing {} shifts...",
        shifts.cash_drawer_shifts.len()
    );

    for shift in shifts.cash_drawer_shifts {
        if shift.state != ShiftState::Closed {
            continue;
        }

        let events: GetShiftEventsResponse = square
            .get(format!(
                "https://connect.squareup.com/v2/cash-drawers/shifts/{}/events",
                shift.id
            ))
            .query(&[("location_id", &args.square_location_id)])
            .send()
            .await?
            .json()
            .await?;

        for event in events.cash_drawer_shift_events {
            if event.event_type != ShiftEventType::PaidOut {
                continue;
            }

            if !args.exclusions.is_empty() && is_excluded(&event, &args.exclusions) {
                continue;
            }

            match invoices.find_match(&event) {
                InvoiceMatchResult::None => {
                    unmatched.push((shift.clone(), event));
                }
                InvoiceMatchResult::AlreadyPaid => {
                    already_paid.push(event);
                }
                InvoiceMatchResult::UnpaidSingle(invoice) => {
                    matched.push((shift.clone(), event, invoice));
                }
                InvoiceMatchResult::UnpaidMultiple(invoices) => {
                    match prompt_invoice_match(&shift, &event, &invoices) {
                        Some(inv) => {
                            matched.push((shift.clone(), event, inv));
                        }
                        None => {
                            unmatched.push((shift.clone(), event));
                        }
                    }
                }
            }
        }
    }

    print_progress(&matched, &already_paid, &unmatched);

    let payment_objects = matched
        .iter()
        .map(|(s, e, i)| PaymentRequestObject {
            invoice: PaymentRequestObjectInvoice {
                invoice_id: i.invoice_id.clone(),
            },
            account: PaymentRequestObjectAccount {
                code: args.xero_payment_account_code.clone(),
            },
            date: s.created_at.date_naive(),
            amount: e.event_money.as_dec(),
            reference: "Auto-reconciled using cashtx tool".to_string(),
        })
        .collect::<Vec<_>>();

    if payment_objects.is_empty() {
        println!("   Done, no payments needed to be submitted");
        return Ok(());
    }

    match xero
        .put("https://api.xero.com/api.xro/2.0/Payments")
        .json(&json!({
            "Payments": payment_objects,
        }))
        .send()
        .await?
        .error_for_status()
    {
        Ok(_) => {
            println!("   Payments submitted successfully");
        }
        Err(err) => eprintln!("   Failed to submit payments: {err}"),
    }

    Ok(())
}

fn is_excluded(event: &ShiftEvent, pattern: &str) -> bool {
    // Shouldn't be compiling regex here but I have no respect for my CPU so fuck it
    let re = Regex::new(pattern).expect("invalid exclusion pattern");
    re.is_match(
        event
            .description
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .trim(),
    )
}

fn print_progress(
    matched: &[(Shift, ShiftEvent, Invoice)],
    already_paid: &[ShiftEvent],
    unmatched: &[(Shift, ShiftEvent)],
) {
    println!();

    println!("   Matched transactions:");
    for (_, e, i) in matched {
        println!(
            "     {} £{} => {} £{}",
            e.description
                .as_deref()
                .map(|s| s.trim())
                .unwrap_or("(no description)"),
            e.event_money,
            i.contact.name,
            i.amount_due
        );
    }
    println!();

    println!("   Matched already paid transactions:");
    for e in already_paid {
        println!(
            "     {} £{}",
            e.description
                .as_deref()
                .map(|s| s.trim())
                .unwrap_or("(no description)"),
            e.event_money,
        );
    }
    println!();

    println!("   Unmatched transactions:");
    for (s, e) in unmatched {
        println!(
            "     {} £{} {}",
            e.description
                .as_deref()
                .map(|s| s.trim())
                .unwrap_or("(no description)"),
            e.event_money,
            s.created_at.format("%Y-%m-%d")
        );
    }
    println!();
}

fn prompt_invoice_match(
    shift: &Shift,
    event: &ShiftEvent,
    invoices: &[Invoice],
) -> Option<Invoice> {
    println!();
    println!(
        "   Pick invoice for cash event: {} {} £{}",
        shift.created_at.format("%Y-%m-%d"),
        event.description.as_deref().unwrap_or("(no description)"),
        event.event_money
    );
    println!();

    for (index, inv) in invoices.iter().enumerate() {
        println!(
            "     #{index} | {} | {} | £{}",
            inv.contact.name,
            inv.due_date.format("%Y-%m-%d"),
            inv.amount_due
        );
    }

    println!("     (empty input to leave undecided)");
    println!();

    let mut chosen = String::new();
    stdin().read_line(&mut chosen).unwrap();

    let chosen = chosen.trim();

    if chosen.is_empty() {
        return None;
    }

    let chosen_int: usize = chosen.parse().unwrap();

    invoices.get(chosen_int).cloned()
}

fn square_client(args: &Args) -> ClientWithMiddleware {
    let mut headers = HeaderMap::new();

    let mut auth_value =
        HeaderValue::from_str(&format!("Bearer {}", args.square_access_token)).unwrap();
    auth_value.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth_value);

    headers.insert("Square-Version", "2025-10-16".parse().unwrap());
    headers.insert("Content-Type", "application/json".parse().unwrap());

    http_client(headers)
}

/// https://api.xero.com/api.xro/2.0/Invoices?Statuses=AUTHORISED&where=Type%3D%3D%22ACCPAY%22%20AND%20AmountDue%3D60.38
async fn xero_client(args: &Args) -> ClientWithMiddleware {
    let mut headers = HeaderMap::new();

    let access_token = get_xero_access_token(args).await.unwrap();

    let mut auth_value = HeaderValue::from_str(&format!("Bearer {}", access_token)).unwrap();
    auth_value.set_sensitive(true);

    headers.insert(AUTHORIZATION, auth_value);
    headers.insert("Accept", "application/json".parse().unwrap());
    headers.insert("Xero-Tenant-Id", args.xero_tenant_id.parse().unwrap());

    http_client(headers)
}

async fn get_xero_access_token(args: &Args) -> Result<String> {
    let mut headers = HeaderMap::new();

    let mut auth_value = HeaderValue::from_str(&format!(
        "Basic {}",
        BASE64_STANDARD.encode(format!(
            "{}:{}",
            args.xero_client_id, args.xero_client_secret
        ))
    ))
    .unwrap();

    auth_value.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, auth_value);

    let scopes = [
        "accounting.transactions",
        "accounting.transactions.read",
        "accounting.reports.read",
        "accounting.reports.tenninetynine.read",
        "accounting.budgets.read",
        "accounting.journals.read",
        "accounting.settings",
        "accounting.settings.read",
        "accounting.contacts",
        "accounting.attachments",
        "accounting.contacts.read",
        "accounting.attachments.read",
    ]
    .join(" ");

    headers.insert(
        "Content-Type",
        "application/x-www-form-urlencoded".parse().unwrap(),
    );

    let client = Client::builder().default_headers(headers).build()?;

    let response: Value = client
        .post("https://identity.xero.com/connect/token")
        .form(&[("grant_type", "client_credentials"), ("scope", &scopes)])
        .send()
        .await?
        .json()
        .await?;

    Ok(response
        .pointer("/access_token")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string())
}
