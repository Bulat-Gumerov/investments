mod common;
mod confirmation;
mod dividends;
mod interest;
mod parsers;
mod taxes;
mod trades;

use std::iter::Iterator;

use csv::{self, StringRecord};
use log::trace;

use crate::brokers::{Broker, BrokerInfo};
use crate::config::Config;
use crate::core::GenericResult;
use crate::currency::Cash;

#[cfg(test)] use super::{BrokerStatement};
use super::{BrokerStatementReader, PartialBrokerStatement};

use self::common::{RecordSpec, Record, RecordParser, format_record};

pub struct StatementReader {
    broker_info: BrokerInfo,
}

impl StatementReader {
    pub fn new(config: &Config) -> GenericResult<Box<dyn BrokerStatementReader>> {
        Ok(Box::new(StatementReader {
            broker_info: Broker::InteractiveBrokers.get_info(config)?,
        }))
    }
}

impl BrokerStatementReader for StatementReader {
    fn is_statement(&self, file_name: &str) -> GenericResult<bool> {
        Ok(file_name.ends_with(".csv"))
    }

    fn read(&self, path: &str) -> GenericResult<PartialBrokerStatement> {
        StatementParser {
            statement: PartialBrokerStatement::new(self.broker_info.clone()),
            base_currency: None,
            base_currency_summary: None,
        }.parse(path)
    }
}

enum State {
    None,
    Record(StringRecord),
    Header(StringRecord),
}

pub struct StatementParser {
    statement: PartialBrokerStatement,
    base_currency: Option<String>,
    base_currency_summary: Option<Cash>,
}

impl StatementParser {
    fn parse(mut self, path: &str) -> GenericResult<PartialBrokerStatement> {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_path(path)?;

        let mut records = reader.records();
        let mut state = Some(State::None);

        'state: loop {
            match state.take().unwrap() {
                State::None => {
                    match records.next() {
                        Some(result) => state = Some(State::Record(result?)),
                        None => break,
                    };
                }
                State::Record(record) => {
                    if record.len() < 2 {
                        return Err!("Invalid record: {}", format_record(&record));
                    }

                    if record.get(1).unwrap() == "Header" {
                        state = Some(State::Header(record));
                    } else if record.get(1).unwrap() == "" {
                        trace!("Headerless record: {}.", format_record(&record));
                        state = Some(State::None);
                    } else {
                        return Err!("Invalid record: {}", format_record(&record));
                    }
                },
                State::Header(record) => {
                    let spec = parse_header(&record);
                    let parser: Box<dyn RecordParser> = match spec.name {
                        "Statement" => Box::new(parsers::StatementInfoParser {}),
                        "Account Information" => Box::new(parsers::AccountInformationParser {}),
                        "Change in NAV" => Box::new(parsers::ChangeInNavParser {}),
                        "Cash Report" => Box::new(parsers::CashReportParser {}),
                        "Open Positions" => Box::new(trades::OpenPositionsParser {}),
                        "Trades" => Box::new(trades::TradesParser {}),
                        "Deposits & Withdrawals" => Box::new(parsers::DepositsAndWithdrawalsParser {}),
                        "Dividends" => Box::new(dividends::DividendsParser {}),
                        "Withholding Tax" => Box::new(taxes::WithholdingTaxParser {}),
                        "Interest" => Box::new(interest::InterestParser {}),
                        "Financial Instrument Information" => Box::new(parsers::FinancialInstrumentInformationParser {}),
                        _ => Box::new(parsers::UnknownRecordParser {}),
                    };

                    let data_types = parser.data_types();
                    let skip_data_types = parser.skip_data_types();
                    let skip_totals = parser.skip_totals();

                    for result in &mut records {
                        let record = result?;
                        if record.len() < 3 {
                            return Err!("Invalid record: {}", format_record(&record));
                        }

                        if record.get(0).unwrap() != spec.name {
                            state = Some(State::Record(record));
                            continue 'state;
                        } else if record.get(1).unwrap() == "Header" {
                            state = Some(State::Header(record));
                            continue 'state;
                        }

                        if let Some(skip_data_types) = skip_data_types {
                            if skip_data_types.contains(&record.get(1).unwrap()) {
                                continue;
                            }
                        }

                        if let Some(data_types) = data_types {
                            if !data_types.contains(&record.get(1).unwrap()) {
                                return Err!("Invalid data record type: {}", format_record(&record));
                            }
                        }

                        // Matches totals records. For example:
                        // * Deposits & Withdrawals,Data,Total,,,1000
                        // * Deposits & Withdrawals,Data,Total in USD,,,1000
                        // * Deposits & Withdrawals,Data,Total Deposits & Withdrawals in USD,,,1000
                        // * Interest,Data,Total,,,100
                        // * Interest,Data,Total in USD,,,100
                        // * Interest,Data,Total Interest in USD,,,100
                        if skip_totals && record.get(2).unwrap().starts_with("Total") {
                            continue;
                        }

                        parser.parse(&mut self, &Record::new(&spec, &record)).map_err(|e| format!(
                            "Failed to parse ({}) record: {}", format_record(&record), e
                        ))?;
                    }

                    break;
                }
            }
        }

        // When statement has no non-base currency activity it contains only base currency summary
        // and we have to use it as the only source of current cash assets info.
        if self.statement.cash_assets.is_empty() {
            let amount = self.base_currency_summary.ok_or_else(||
                "Unable to find base currency summary")?;

            self.statement.cash_assets.deposit(amount);
        }

        self.statement.validate()
    }
}

fn parse_header(record: &StringRecord) -> RecordSpec {
    let offset = 2;
    let name = record.get(0).unwrap();
    let fields = record.iter().skip(offset).collect::<Vec<_>>();
    trace!("Header: {}: {}.", name, format_record(fields.iter().cloned()));
    RecordSpec::new(name, fields, offset)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use super::*;

    #[test]
    fn parse_real_empty() {
        let statement = parse_full("empty");

        assert!(statement.cash_flows.is_empty());
        assert!(!statement.cash_assets.is_empty());
        assert!(statement.idle_cash_interest.is_empty());

        assert!(statement.stock_buys.is_empty());
        assert!(statement.stock_sells.is_empty());
        assert!(statement.dividends.is_empty());

        assert!(statement.open_positions.is_empty());
        assert!(statement.instrument_names.is_empty());
    }

    #[test]
    fn parse_real_current() {
        let statement = parse_full("current");

        assert!(!statement.cash_flows.is_empty());
        assert!(!statement.cash_assets.is_empty());
        assert!(!statement.idle_cash_interest.is_empty());

        assert!(!statement.stock_buys.is_empty());
        assert!(!statement.stock_sells.is_empty());

        assert!(!statement.dividends.is_empty());
        assert!(statement.dividends.iter().any(|dividend| dividend.paid_tax.is_positive()));

        assert!(!statement.open_positions.is_empty());
        assert!(!statement.instrument_names.is_empty());
    }

    #[rstest(name => ["return-of-capital-with-tax", "return-of-capital-without-tax"])]
    fn parse_real(name: &str) {
        parse_full(name);
    }

    fn parse_full(name: &str) -> BrokerStatement {
        let path = format!("testdata/interactive-brokers/{}", name);
        BrokerStatement::read(&Config::mock(), Broker::InteractiveBrokers, &path).unwrap()
    }

    #[rstest(name => ["no-activity", "multi-currency-activity"])]
    fn parse_real_partial(name: &str) {
        let path = format!("testdata/interactive-brokers/partial/{}.csv", name);
        StatementReader::new(&Config::mock()).unwrap().read(&path).unwrap();
    }
}