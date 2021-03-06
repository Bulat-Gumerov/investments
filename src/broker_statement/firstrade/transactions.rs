use log::warn;
use num_traits::Zero;
use serde::Deserialize;

use crate::broker_statement::{StockBuy, StockSell, IdleCashInterest, Dividend};
use crate::core::EmptyResult;
use crate::currency::{Cash, CashAssets};
use crate::formatting;
use crate::localities;
use crate::types::{Date, Decimal};
use crate::util::{self, DecimalRestrictions};

use super::StatementParser;
use super::common::{Ignore, deserialize_date, deserialize_decimal, validate_sub_account};
use super::security_info::{SecurityInfo, SecurityId, SecurityType};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transactions {
    #[serde(rename = "DTSTART", deserialize_with = "deserialize_date")]
    pub start_date: Date,

    #[serde(rename = "DTEND", deserialize_with = "deserialize_date")]
    pub end_date: Date,

    #[serde(rename = "INVBANKTRAN", default)]
    cash_flows: Vec<CashFlowInfo>,

    #[serde(rename = "BUYSTOCK", default)]
    stock_buys: Vec<StockBuyInfo>,

    // Dividend reinvestment transactions appear here
    #[serde(rename = "BUYOTHER", default)]
    other_buys: Vec<OtherBuyInfo>,

    #[serde(rename = "SELLSTOCK", default)]
    stock_sells: Vec<StockSellInfo>,

    #[serde(rename = "INCOME", default)]
    income: Vec<IncomeInfo>,
}

impl Transactions {
    pub fn parse(
        self, parser: &mut StatementParser, currency: &str, securities: &SecurityInfo,
    ) -> EmptyResult {
        let mut ffs_balance = dec!(0);

        for cash_flow in self.cash_flows {
            cash_flow.parse(parser, &mut ffs_balance, currency)?;
        }

        if !ffs_balance.is_zero() {
            return Err!("Got a non-zero FFS balance: {}", ffs_balance);
        }

        for stock_buy in self.stock_buys {
            if stock_buy._type != "BUY" {
                return Err!("Got an unsupported type of stock purchase: {:?}", stock_buy._type);
            }
            stock_buy.transaction.parse(parser, currency, securities, true)?;
        }

        for other_buy in self.other_buys {
            other_buy.transaction.parse(parser, currency, securities, true)?;
        }

        for stock_sell in self.stock_sells {
            if stock_sell._type != "SELL" {
                return Err!("Got an unsupported type of stock sell: {:?}", stock_sell._type);
            }
            stock_sell.transaction.parse(parser, currency, securities, false)?;
        }

        for income in self.income {
            income.parse(parser, currency, securities)?;
        }

        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CashFlowInfo {
    #[serde(rename = "STMTTRN")]
    transaction: CashFlowTransaction,
    #[serde(rename = "SUBACCTFUND")]
    sub_account: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CashFlowTransaction {
    #[serde(rename = "TRNTYPE")]
    _type: String,
    #[serde(rename = "DTPOSTED", deserialize_with = "deserialize_date")]
    date: Date,
    #[serde(rename = "TRNAMT", deserialize_with = "deserialize_decimal")]
    amount: Decimal,
    #[serde(rename = "FITID")]
    id: String,
    #[serde(rename = "NAME")]
    name: String,
}

impl CashFlowInfo {
    fn parse(self, parser: &mut StatementParser, ffs_balance: &mut Decimal, currency: &str) -> EmptyResult {
        let transaction = self.transaction;

        // These are some service transactions related to Securities Lending Income Program.
        // They shouldn't affect account balance and always compensate each other.
        match transaction.name.as_str() {
            "XFER CASH FROM FFS" | "XFER FFS TO CASH" => {
                *ffs_balance += transaction.amount;
                return Ok(());
            }
            _ => ()
        };

        if transaction._type != "CREDIT" {
            return Err!(
                "Got {:?} cash flow transaction of an unsupported type: {}",
                transaction.id, transaction._type);
        }
        validate_sub_account(&self.sub_account)?;

        let amount = util::validate_named_decimal(
            "transaction amount", transaction.amount, DecimalRestrictions::StrictlyPositive)?;
        parser.statement.cash_flows.push(CashAssets::new(transaction.date, currency, amount));

        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StockBuyInfo {
    #[serde(rename = "BUYTYPE")]
    _type: String,
    #[serde(rename = "INVBUY")]
    transaction: StockTradeTransaction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OtherBuyInfo {
    #[serde(rename = "INVBUY")]
    transaction: StockTradeTransaction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StockSellInfo {
    #[serde(rename = "SELLTYPE")]
    _type: String,
    #[serde(rename = "INVSELL")]
    transaction: StockTradeTransaction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StockTradeTransaction {
    #[serde(rename = "INVTRAN")]
    info: TransactionInfo,
    #[serde(rename = "SECID")]
    security_id: SecurityId,
    #[serde(rename = "UNITS", deserialize_with = "deserialize_decimal")]
    units: Decimal,
    #[serde(rename = "UNITPRICE", deserialize_with = "deserialize_decimal")]
    price: Decimal,
    #[serde(rename = "COMMISSION", deserialize_with = "deserialize_decimal")]
    commission: Decimal,
    #[serde(rename = "FEES", deserialize_with = "deserialize_decimal")]
    fees: Decimal,
    #[serde(rename = "TOTAL", deserialize_with = "deserialize_decimal")]
    total: Decimal,
    #[serde(rename = "SUBACCTSEC")]
    sub_account_to: String,
    #[serde(rename = "SUBACCTFUND")]
    sub_account_from: String,
}

impl StockTradeTransaction {
    fn parse(
        self, parser: &mut StatementParser, currency: &str, securities: &SecurityInfo, buy: bool,
    ) -> EmptyResult {
        validate_sub_account(&self.sub_account_from)?;
        validate_sub_account(&self.sub_account_to)?;

        let symbol = match securities.get(&self.security_id)? {
            SecurityType::Stock(symbol) => symbol,
            _ => return Err!("Got {} stock trade with an unexpected security type", self.security_id),
        };

        let quantity = util::validate_named_decimal("trade quantity", self.units, if buy {
            DecimalRestrictions::StrictlyPositive
        } else {
            DecimalRestrictions::StrictlyNegative
        })?.abs().normalize();

        let price = util::validate_named_decimal(
            "price", self.price, DecimalRestrictions::StrictlyPositive)
            .map(|price| Cash::new(currency, price.normalize()))?;

        let commission = util::validate_named_decimal(
            "commission", self.commission, DecimalRestrictions::PositiveOrZero
        ).and_then(|commission| {
            let fees = util::validate_named_decimal(
                "fees", self.fees, DecimalRestrictions::PositiveOrZero)?;
            Ok(commission + fees)
        }).map(|commission| Cash::new(currency, commission))?;

        let volume = util::validate_named_decimal(
            "trade volume", self.total, if buy {
                DecimalRestrictions::StrictlyNegative
            } else {
                DecimalRestrictions::StrictlyPositive
            })
            .map(|mut volume| {
                volume = volume.abs();

                if buy {
                    volume -= commission.amount;
                } else {
                    volume += commission.amount
                }

                Cash::new(currency, volume)
            })?;
        debug_assert_eq!(volume, (price * quantity).round());

        if buy {
            parser.statement.stock_buys.push(StockBuy::new(
                &symbol, quantity, price, volume, commission,
                self.info.conclusion_date, self.info.execution_date));
        } else {
            parser.statement.stock_sells.push(StockSell::new(
                &symbol, quantity, price, volume, commission,
                self.info.conclusion_date, self.info.execution_date, false));
        }

        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomeInfo {
    #[serde(rename = "INVTRAN")]
    info: TransactionInfo,
    #[serde(rename = "SECID")]
    security_id: SecurityId,
    #[serde(rename = "INCOMETYPE")]
    _type: String,
    #[serde(rename = "TOTAL", deserialize_with = "deserialize_decimal")]
    total: Decimal,
    #[serde(rename = "SUBACCTSEC")]
    sub_account_to: String,
    #[serde(rename = "SUBACCTFUND")]
    sub_account_from: String,
}

impl IncomeInfo {
    fn parse(
        self, parser: &mut StatementParser, currency: &str, securities: &SecurityInfo,
    ) -> EmptyResult {
        validate_sub_account(&self.sub_account_from)?;
        validate_sub_account(&self.sub_account_to)?;

        let date = self.info.conclusion_date;
        if self.info.execution_date != date {
            return Err!("Got an unexpected {:?} income settlement date: {} -> {}",
                self.info.memo, formatting::format_date(date),
                formatting::format_date(self.info.execution_date));
        }

        match (self._type.as_str(), securities.get(&self.security_id)?) {
            ("MISC", SecurityType::Interest) => {
                let amount = util::validate_named_decimal(
                    "idle cash interest amount", self.total, DecimalRestrictions::NonZero)?;

                let interest = IdleCashInterest::new(date, Cash::new(currency, amount));
                parser.statement.idle_cash_interest.push(interest);
            },
            ("DIV", SecurityType::Stock(issuer)) => {
                let amount = util::validate_named_decimal(
                    "dividend amount", self.total, DecimalRestrictions::StrictlyPositive)?;
                self.parse_dividend(parser, &issuer, Cash::new(currency, amount))?;
            },
            _ => return Err!("Got an unsupported income: {:?}", self.info.memo),
        };

        Ok(())
    }

    fn parse_dividend(self, parser: &mut StatementParser, issuer: &str, income: Cash) -> EmptyResult {
        let date = self.info.conclusion_date;
        let currency = income.currency;
        let income = income.amount;

        if parser.reader.warn_on_missing_dividend_details {
            warn!(concat!(
                "There are no detailed information for some dividends - it will be deduced ",
                "approximately. First occurred dividend: {} at {}."
            ), issuer, formatting::format_date(date));

            parser.reader.warn_on_missing_dividend_details = false;
        }

        if self.info.memo.ends_with(" NON-QUALIFIED DIVIDEND NON-RES TAX WITHHELD") {
            return Err!("Got an unexpected dividend description: {:?}", self.info.memo);
        }

        let foreign_country = localities::us();
        let amount = foreign_country.deduce_income(income);
        let paid_tax = amount - income;
        debug_assert_eq!(paid_tax, foreign_country.tax_to_pay(amount, None));

        parser.statement.dividends.push(Dividend {
            date: date,
            issuer: issuer.to_owned(),
            amount: Cash::new(currency, amount),
            paid_tax: Cash::new(currency, paid_tax),
        });

        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionInfo {
    #[serde(rename = "FITID")]
    _id: Ignore,
    #[serde(rename = "DTTRADE", deserialize_with = "deserialize_date")]
    conclusion_date: Date,
    #[serde(rename = "DTSETTLE", deserialize_with = "deserialize_date")]
    execution_date: Date,
    #[serde(rename = "MEMO")]
    memo: String,
}