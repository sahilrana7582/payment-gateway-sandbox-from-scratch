use core::fmt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "db", derive(sqlx::Type))]
#[cfg_attr(feature = "db", sqlx(type_name = "payment_method_type", rename_all = "snake_case"))]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethodType {
    Card,
}

impl PaymentMethodType {
    pub const fn as_str(self) -> &'static str {
        match self {
            PaymentMethodType::Card => "card",
        }
    }
}

impl fmt::Display for PaymentMethodType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
