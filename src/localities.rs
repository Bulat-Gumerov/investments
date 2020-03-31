use chrono::{Datelike, Duration};

use num_traits::Zero;

use crate::currency;
use crate::types::{Date, Decimal};

#[derive(Clone, Copy)]
pub struct Country {
    pub currency: &'static str,
    tax_rate: Decimal,
    tax_precision: u32,
}

impl Country {
    pub fn round_tax(&self, tax: Decimal) -> Decimal {
        // TODO: It looks like Декларация program rounds tax amount to rubles as
        // round_to(round_to(value, 2), 0) because it rounds 10.64 * 65.4244 * 0.13
        // (which is 90.4956) to 91. Don't follow this logic for now - look into the next version.

        currency::round_to(tax, self.tax_precision)
    }

    pub fn tax_to_pay(&self, income: Decimal, paid_tax: Option<Decimal>) -> Decimal {
        if income.is_sign_negative() || income.is_zero() {
            return dec!(0);
        }

        let tax_to_pay = self.round_tax(income * self.tax_rate);

        if let Some(paid_tax) = paid_tax {
            assert!(!paid_tax.is_sign_negative());
            let tax_deduction = self.round_tax(paid_tax);

            if tax_deduction < tax_to_pay {
                tax_to_pay - tax_deduction
            } else {
                dec!(0)
            }
        } else {
            tax_to_pay
        }
    }
}

pub fn russia() -> Country {
    Country {
        currency: "RUB",
        tax_rate: Decimal::new(13, 2),
        tax_precision: 0,
    }
}

pub fn get_russian_stock_exchange_min_last_working_day(today: Date) -> Date {
    if today.month() == 1 && today.day() < 10 {
        Date::from_ymd(today.year() - 1, 12, 30)
    } else if today.month() == 3 && today.day() == 12 {
        today - Duration::days(4)
    } else if today.month() == 5 && today.day() >= 3 && today.day() <= 13 {
        today - Duration::days(5)
    } else {
        // FIXME: Unable to find USD currency rate for 01.04.2020 with 3 days precision
        today - Duration::days(7)
    }
}