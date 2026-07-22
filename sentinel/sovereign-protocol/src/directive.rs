use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Directive {
    pub job: String,
    pub scope: String,
    pub stop_condition: String,
    pub return_expectation: String,
}
