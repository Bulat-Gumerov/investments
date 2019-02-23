use std::{self, fs};
use std::collections::HashMap;
use std::path::Path;

use chrono::Duration;
use log::{debug, warn};

use crate::brokers::BrokerInfo;
use crate::config::{Config, Broker};
use crate::core::{EmptyResult, GenericResult};
use crate::currency::{Cash, CashAssets, MultiCurrencyCashAccount};
use crate::formatting;
use crate::quotes::Quotes;
use crate::types::{Date, Decimal};
use crate::util;

use self::dividends::Dividend;
use self::partial::PartialBrokerStatement;
use self::taxes::{TaxId, TaxChanges};
use self::trades::{StockBuy, StockSell, StockSellSource};

mod dividends;
mod ib;
mod open_broker;
mod partial;
mod taxes;
mod trades;

#[derive(Debug)]
pub struct BrokerStatement {
    pub broker: BrokerInfo,
    pub period: (Date, Date),

    pub cash_flows: Vec<CashAssets>,
    pub cash_assets: MultiCurrencyCashAccount,

    pub stock_buys: Vec<StockBuy>,
    pub stock_sells: Vec<StockSell>,
    pub dividends: Vec<Dividend>,

    pub open_positions: HashMap<String, u32>,
    instrument_names: HashMap<String, String>,
}

impl BrokerStatement {
    pub fn read(config: &Config, broker: Broker, statement_dir_path: &str) -> GenericResult<BrokerStatement> {
        let statement_reader = match broker {
            Broker::InteractiveBrokers => ib::StatementReader::new(config),
            Broker::OpenBroker => open_broker::StatementReader::new(config),
        }?;

        let mut file_names = get_statement_files(statement_dir_path, statement_reader.as_ref())
            .map_err(|e| format!("Error while reading {:?}: {}", statement_dir_path, e))?;

        if file_names.is_empty() {
            return Err!("{:?} doesn't contain any broker statement", statement_dir_path);
        }

        file_names.sort();

        let mut statements = Vec::new();

        for file_name in &file_names {
            let path = Path::new(statement_dir_path).join(file_name);
            let path = path.to_str().unwrap();

            let statement = statement_reader.read(path).map_err(|e| format!(
                "Error while reading {:?} broker statement: {}", path, e))?;

            statements.push(statement);
        }

        let joint_statement = BrokerStatement::new_from(statements)?;
        debug!("{:#?}", joint_statement);
        Ok(joint_statement)
    }

    fn new_from(mut statements: Vec<PartialBrokerStatement>) -> GenericResult<BrokerStatement> {
        statements.sort_by(|a, b| a.period.unwrap().0.cmp(&b.period.unwrap().0));

        let mut joint_statement = BrokerStatement::new_empty_from(statements.first().unwrap())?;
        let mut dividends_without_paid_tax = Vec::new();
        let mut tax_changes = HashMap::new();

        for mut statement in statements.drain(..) {
            dividends_without_paid_tax.extend(statement.dividends_without_paid_tax.drain(..));

            for (tax_id, changes) in statement.tax_changes.drain() {
                tax_changes.entry(tax_id)
                    .and_modify(|existing: &mut TaxChanges| existing.merge(&changes))
                    .or_insert(changes);
            }

            joint_statement.merge(statement).map_err(|e| format!(
                "Failed to merge broker statements: {}", e))?;
        }

        let mut taxes = HashMap::new();

        for (tax_id, changes) in tax_changes {
            let amount = changes.get_result_tax().map_err(|e| format!(
                "Failed to process {} / {:?} tax: {}",
                formatting::format_date(tax_id.date), tax_id.description, e))?;

            taxes.insert(tax_id, amount);
        }

        for dividend in dividends_without_paid_tax {
            joint_statement.dividends.push(dividend.upgrade(&mut taxes)?);
        }

        if !taxes.is_empty() {
            let taxes = taxes.keys()
                .map(|tax: &taxes::TaxId| format!(
                    "* {date}: {description}", date=formatting::format_date(tax.date),
                    description=tax.description))
                .collect::<Vec<_>>()
                .join("\n");

            return Err!("Unable to find origin operations for the following taxes:\n{}", taxes);
        }

        joint_statement.validate()?;
        joint_statement.process_trades()?;

        Ok(joint_statement)
    }

    fn new_empty_from(statement: &PartialBrokerStatement) -> GenericResult<BrokerStatement> {
        let mut period = statement.get_period()?;
        period.1 = period.0;

        if statement.get_starting_assets()? {
            return Err!("Invalid broker statement period: It has a non-zero starting assets");
        }

        Ok(BrokerStatement {
            broker: statement.broker.clone(),
            period: period,

            cash_flows: Vec::new(),
            cash_assets: MultiCurrencyCashAccount::new(),

            stock_buys: Vec::new(),
            stock_sells: Vec::new(),
            dividends: Vec::new(),

            open_positions: HashMap::new(),
            instrument_names: HashMap::new(),
        })
    }

    pub fn check_date(&self) {
        let date = self.period.1 - Duration::days(1);
        let days = (util::today() - date).num_days();
        let months = Decimal::from(days) / dec!(30);

        if months >= dec!(1) {
            warn!("The broker statement is {} months old and may be outdated.",
                  util::round_to(months, 1));
        }
    }

    pub fn get_instrument_name(&self, symbol: &str) -> GenericResult<String> {
        let name = self.instrument_names.get(symbol).ok_or_else(|| format!(
            "Unable to find {:?} instrument name in the broker statement", symbol))?;
        Ok(format!("{} ({})", name, symbol))
    }

    pub fn batch_quotes(&self, quotes: &mut Quotes) {
        for symbol in self.instrument_names.keys() {
            quotes.batch(&symbol);
        }
    }

    pub fn emulate_sell_order(&mut self, symbol: &str, quantity: u32, price: Cash) -> EmptyResult {
        let today = util::today();

        let conclusion_date = today;
        let execution_date = match self.stock_sells.last() {
            Some(last_trade) if last_trade.execution_date > today => last_trade.execution_date,
            _ => today,
        };

        let commission = self.broker.get_trade_commission(quantity, price)?;

        let stock_cell = StockSell::new(
            symbol, quantity, price, commission, conclusion_date, execution_date);

        self.stock_sells.push(stock_cell);
        self.cash_assets.deposit(price * quantity);
        self.cash_assets.withdraw(commission);

        Ok(())
    }

    pub fn process_trades(&mut self) -> EmptyResult {
        let stock_buys_num = self.stock_buys.len();
        let mut stock_buys = Vec::with_capacity(stock_buys_num);
        let mut unsold_stock_buys: HashMap<String, Vec<StockBuy>> = HashMap::new();

        for stock_buy in self.stock_buys.drain(..).rev() {
            if stock_buy.is_sold() {
                stock_buys.push(stock_buy);
                continue;
            }

            let symbol_buys = match unsold_stock_buys.get_mut(&stock_buy.symbol) {
                Some(symbol_buys) => symbol_buys,
                None => {
                    unsold_stock_buys.insert(stock_buy.symbol.clone(), Vec::new());
                    unsold_stock_buys.get_mut(&stock_buy.symbol).unwrap()
                },
            };

            symbol_buys.push(stock_buy);
        }

        for stock_sell in self.stock_sells.iter_mut() {
            if stock_sell.is_processed() {
                continue;
            }

            let mut remaining_quantity = stock_sell.quantity;
            let mut sources = Vec::new();

            let symbol_buys = unsold_stock_buys.get_mut(&stock_sell.symbol).ok_or_else(|| format!(
                "Error while processing {} position closing: There are no open positions for it",
                stock_sell.symbol
            ))?;

            while remaining_quantity > 0 {
                let mut stock_buy = symbol_buys.pop().ok_or_else(|| format!(
                    "Error while processing {} position closing: There are no open positions for it",
                    stock_sell.symbol
                ))?;

                let sell_quantity = std::cmp::min(remaining_quantity, stock_buy.get_unsold());
                assert!(sell_quantity > 0);

                sources.push(StockSellSource {
                    quantity: sell_quantity,
                    price: stock_buy.price,
                    commission: stock_buy.commission / stock_buy.quantity * sell_quantity,

                    conclusion_date: stock_buy.conclusion_date,
                    execution_date: stock_buy.execution_date,
                });

                remaining_quantity -= sell_quantity;
                stock_buy.sell(sell_quantity);

                if stock_buy.is_sold() {
                    stock_buys.push(stock_buy);
                } else {
                    symbol_buys.push(stock_buy);
                }
            }

            stock_sell.process(sources);
        }

        for (_, mut symbol_buys) in unsold_stock_buys.drain() {
            stock_buys.extend(symbol_buys.drain(..));
        }
        drop(unsold_stock_buys);

        assert_eq!(stock_buys.len(), stock_buys_num);
        self.stock_buys = stock_buys;
        self.order_stock_buys()?;

        self.validate_open_positions()?;

        Ok(())
    }

    fn merge(&mut self, mut statement: PartialBrokerStatement) -> EmptyResult {
        let period = statement.get_period()?;

        if period.0 != self.period.1 {
            return Err!("Non-continuous periods: {}, {}",
                formatting::format_period(self.period.0, self.period.1),
                formatting::format_period(period.0, period.1));
        }

        self.period.1 = period.1;

        self.cash_flows.extend(statement.cash_flows.drain(..));
        self.cash_assets = statement.cash_assets;

        self.stock_buys.extend(statement.stock_buys.drain(..));
        self.stock_sells.extend(statement.stock_sells.drain(..));
        self.dividends.extend(statement.dividends.drain(..));

        self.open_positions = statement.open_positions;
        self.instrument_names.extend(statement.instrument_names.drain());

        Ok(())
    }

    fn validate(&mut self) -> EmptyResult {
        self.cash_flows.sort_by_key(|cash_flow| cash_flow.date);
        self.dividends.sort_by_key(|dividend| dividend.date);

        self.order_stock_buys()?;
        self.order_stock_sells()?;

        let min_date = self.period.0;
        let max_date = self.period.1 - Duration::days(1);
        let validate_date = |name, first_date, last_date| -> EmptyResult {
            if first_date < min_date {
                return Err!("Got a {} outside of statement period: {}",
                    name, formatting::format_date(first_date));
            }

            if last_date > max_date {
                return Err!("Got a {} outside of statement period: {}",
                    name, formatting::format_date(first_date));
            }

            Ok(())
        };

        if !self.cash_flows.is_empty() {
            let first_date = self.cash_flows.first().unwrap().date;
            let last_date = self.cash_flows.last().unwrap().date;
            validate_date("cash flow", first_date, last_date)?;
        }

        if !self.stock_buys.is_empty() {
            let first_date = self.stock_buys.first().unwrap().conclusion_date;
            let last_date = self.stock_buys.last().unwrap().conclusion_date;
            validate_date("stock buy", first_date, last_date)?;
        }

        if !self.stock_sells.is_empty() {
            let first_date = self.stock_sells.first().unwrap().conclusion_date;
            let last_date = self.stock_sells.last().unwrap().conclusion_date;
            validate_date("stock sell", first_date, last_date)?;
        }

        if !self.dividends.is_empty() {
            let first_date = self.dividends.first().unwrap().date;
            let last_date = self.dividends.last().unwrap().date;
            validate_date("dividend", first_date, last_date)?;
        }

        Ok(())
    }

    fn order_stock_buys(&mut self) -> EmptyResult {
        self.stock_buys.sort_by_key(|trade| (trade.conclusion_date, trade.execution_date));

        let mut prev_execution_date = None;

        for stock_buy in &self.stock_buys {
            if let Some(prev_execution_date) = prev_execution_date {
                if stock_buy.execution_date < prev_execution_date {
                    return Err!("Got an unexpected execution order for buy trades");
                }
            }

            prev_execution_date = Some(stock_buy.execution_date);
        }

        Ok(())
    }

    fn order_stock_sells(&mut self) -> EmptyResult {
        self.stock_sells.sort_by_key(|trade| (trade.conclusion_date, trade.execution_date));

        let mut prev_execution_date = None;

        for stock_sell in &self.stock_sells {
            if let Some(prev_execution_date) = prev_execution_date {
                if stock_sell.execution_date < prev_execution_date {
                    return Err!("Got an unexpected execution order for sell trades");
                }
            }

            prev_execution_date = Some(stock_sell.execution_date);
        }

        Ok(())
    }

    fn validate_open_positions(&self) -> EmptyResult {
        let mut open_positions = HashMap::new();

        for stock_buy in &self.stock_buys {
            if stock_buy.is_sold() {
                continue;
            }

            let quantity = stock_buy.get_unsold();

            if let Some(position) = open_positions.get_mut(&stock_buy.symbol) {
                *position += quantity;
            } else {
                open_positions.insert(stock_buy.symbol.clone(), quantity);
            }
        }

        if open_positions != self.open_positions {
            return Err!("The calculated open positions don't match declared ones in the statement");
        }

        Ok(())
    }
}

fn get_statement_files(
    statement_dir_path: &str, statement_reader: &BrokerStatementReader
) -> GenericResult<Vec<String>> {
    let mut file_names = Vec::new();

    for entry in fs::read_dir(statement_dir_path)? {
        let file_name = entry?.file_name().into_string().map_err(|file_name| format!(
            "Got an invalid file name: {:?}", file_name.to_string_lossy()))?;

        if statement_reader.is_statement(&file_name) {
            file_names.push(file_name);
        }
    }

    Ok(file_names)
}

pub trait BrokerStatementReader {
    fn is_statement(&self, file_name: &str) -> bool;
    fn read(&self, path: &str) -> GenericResult<PartialBrokerStatement>;
}