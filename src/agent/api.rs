use std::time::Duration;

use crate::agent::Capabilities;
use crate::crates::Crate;
use crate::experiments::Experiment;
use crate::prelude::*;
use crate::results::TestResult;
use crate::server::api_types::{AgentConfig, ApiResponse, CraterToken};
use crate::toolchain::Toolchain;
use crate::utils;
use rand::Rng;
use reqwest::blocking::RequestBuilder;
use reqwest::header::AUTHORIZATION;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::json;

#[derive(Debug, Fail)]
pub enum AgentApiError {
    #[fail(display = "invalid API endpoint called")]
    InvalidEndpoint,
    #[fail(display = "Crater server unavailable")]
    ServerUnavailable,
    #[fail(display = "payload sent to the server too large")]
    PayloadTooLarge,
    #[fail(display = "invalid authorization token")]
    InvalidAuthorizationToken,
    #[fail(display = "internal server error: {}", _0)]
    InternalServerError(String),
}

trait ResponseExt {
    fn to_api_response<T: DeserializeOwned>(self) -> Fallible<T>;
}

impl ResponseExt for ::reqwest::blocking::Response {
    fn to_api_response<T: DeserializeOwned>(self) -> Fallible<T> {
        // 404 responses are not JSON, so avoid parsing them
        match self.status() {
            StatusCode::NOT_FOUND => return Err(AgentApiError::InvalidEndpoint.into()),
            StatusCode::BAD_GATEWAY
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT => {
                return Err(AgentApiError::ServerUnavailable.into());
            }
            StatusCode::PAYLOAD_TOO_LARGE => return Err(AgentApiError::PayloadTooLarge.into()),
            _ => {}
        }

        let status = self.status();
        let result: ApiResponse<T> = self
            .json()
            .with_context(|_| format!("failed to parse API response (status code {})", status,))?;
        match result {
            ApiResponse::Success { result } => Ok(result),
            ApiResponse::SlowDown => Err(AgentApiError::ServerUnavailable.into()),
            ApiResponse::InternalError { error } => {
                Err(AgentApiError::InternalServerError(error).into())
            }
            ApiResponse::Unauthorized => Err(AgentApiError::InvalidAuthorizationToken.into()),
            ApiResponse::NotFound => Err(AgentApiError::InvalidEndpoint.into()),
        }
    }
}

pub struct AgentApi {
    url: String,
    token: String,
}

impl AgentApi {
    pub fn new(url: &str, token: &str) -> Self {
        AgentApi {
            url: url.to_string(),
            token: token.to_string(),
        }
    }

    fn build_request(&self, method: Method, url: &str) -> RequestBuilder {
        utils::http::prepare_sync(method, &format!("{}/agent-api/{}", self.url, url)).header(
            AUTHORIZATION,
            (CraterToken {
                token: self.token.clone(),
            })
            .to_string(),
        )
    }

    fn retry<T, F: Fn(&Self) -> Fallible<T>>(&self, f: F) -> Fallible<T> {
        let mut retry_interval = 16u64;
        loop {
            match f(self) {
                Ok(res) => return Ok(res),
                Err(err) => {
                    let retry = if let Some(AgentApiError::ServerUnavailable) = err.downcast_ref() {
                        true
                    } else if let Some(err) = err.downcast_ref::<::reqwest::Error>() {
                        err.is_timeout() || err.is_connect()
                    } else {
                        // We retry these errors. Ideally it's something the
                        // server would handle, but that's (unfortunately) hard
                        // in practice.
                        format!("{:?}", err).contains("database is locked")
                    };

                    if retry {
                        let sleep_for = Duration::from_millis(
                            rand::thread_rng().gen_range(500..(retry_interval * 1000)),
                        );
                        warn!(
                            "connection to the server failed. retrying in {:?}...",
                            sleep_for
                        );
                        ::std::thread::sleep(sleep_for);
                        retry_interval *= 2;
                        if retry_interval >= 8 * 60 {
                            retry_interval = 8 * 60;
                        }

                        continue;
                    }

                    return Err(err);
                }
            }
        }
    }

    pub fn config(&self, caps: &Capabilities) -> Fallible<AgentConfig> {
        self.retry(|this| {
            this.build_request(Method::POST, "config")
                .json(&json!(caps))
                .send()?
                .to_api_response()
        })
    }

    pub fn next_experiment(&self) -> Fallible<Experiment> {
        self.retry(|this| loop {
            let resp: Option<_> = this
                .build_request(Method::POST, "next-experiment")
                .send()?
                .to_api_response()?;

            if let Some(experiment) = resp {
                return Ok(experiment);
            }

            // If we're just waiting for an experiment, we should be considered
            // healthy.
            crate::agent::set_healthy();

            ::std::thread::sleep(Duration::from_secs(120));
        })
    }

    pub fn next_crate(&self, ex: &str) -> Fallible<Option<Crate>> {
        self.retry(|this| {
            let resp: Option<Crate> = this
                .build_request(Method::POST, "next-crate")
                .json(&json!(ex))
                .send()?
                .to_api_response()?;

            Ok(resp)
        })
    }

    pub fn record_progress(
        &self,
        ex: &Experiment,
        krate: &Crate,
        toolchain: &Toolchain,
        log: &[u8],
        result: &TestResult,
        version: Option<(&Crate, &Crate)>,
    ) -> Fallible<()> {
        self.retry(|this| {
            let _: bool = this
                .build_request(Method::POST, "record-progress")
                .json(&json!({
                    "experiment-name": ex.name,
                    "results": [
                        {
                            "crate": krate,
                            "toolchain": toolchain,
                            "result": result,
                            "log": base64::encode(log),
                        },
                    ],
                    "version": version
                }))
                .send()?
                .to_api_response()?;
            Ok(())
        })
    }

    pub fn heartbeat(&self) -> Fallible<()> {
        self.retry(|this| {
            let _: bool = this
                .build_request(Method::POST, "heartbeat")
                .send()?
                .to_api_response()?;
            Ok(())
        })
    }

    pub fn report_error(&self, ex: &Experiment, error: String) -> Fallible<()> {
        self.retry(|this| {
            let _: bool = this
                .build_request(Method::POST, "error")
                .json(&json!({
                    "experiment-name": ex.name,
                    "error": error
                }))
                .send()?
                .to_api_response()?;
            Ok(())
        })
    }
}
