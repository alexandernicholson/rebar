use serde::{Deserialize, Serialize};

pub fn fib(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 2..=n {
        let c = a.wrapping_add(b);
        a = b;
        b = c;
    }
    b
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ComputeRequest {
    pub n: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ComputeResponse {
    pub result: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoreValue {
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoreResponse {
    pub key: String,
    pub value: String,
}

pub const GATEWAY_HTTP_PORT: u16 = 8080;
pub const COMPUTE_HTTP_PORT: u16 = 8081;
pub const STORE_HTTP_PORT: u16 = 8082;
