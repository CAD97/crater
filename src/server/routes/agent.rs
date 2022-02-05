use crate::agent::Capabilities;
use crate::experiments::{Assignee, Experiment, Status};
use crate::prelude::*;
use crate::results::{DatabaseDB, EncodingType, ProgressData};
use crate::server::api_types::{AgentConfig, ApiResponse};
use crate::server::auth::{auth_filter, AuthDetails, TokenType};
use crate::server::messages::Message;
use crate::server::{Data, GithubData, HttpError};
use failure::Compat;
use http::{Response, StatusCode};
use hyper::Body;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use warp::{self, Filter, Rejection};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ExperimentData<T> {
    experiment_name: String,
    #[serde(flatten)]
    data: T,
}

pub fn routes(
    data: Arc<Data>,
    mutex: Arc<Mutex<Data>>,
    github_data: Option<Arc<GithubData>>,
) -> impl Filter<Extract = (Response<Body>,), Error = Rejection> + Clone {
    let data_cloned = data.clone();
    let data_filter = warp::any().map(move || data_cloned.clone());
    let mutex_filter = warp::any().map(move || mutex.clone());
    let github_data_filter = warp::any().map(move || github_data.clone());

    let config = warp::post2()
        .and(warp::path("config"))
        .and(warp::path::end())
        .and(warp::body::json())
        .and(data_filter.clone())
        .and(auth_filter(data.clone(), TokenType::Agent))
        .map(endpoint_config);

    // Assume agents that do not POST their capabilities to `/config` are Linux agents.
    let config_old = warp::get2()
        .and(warp::path("config"))
        .and(warp::path::end())
        .and(data_filter.clone())
        .and(auth_filter(data.clone(), TokenType::Agent))
        .map(|data, auth| endpoint_config(Capabilities::new(&["linux"]), data, auth));

    let next_experiment = warp::get2()
        .and(warp::path("next-experiment"))
        .and(warp::path::end())
        .and(mutex_filter.clone())
        .and(github_data_filter)
        .and(auth_filter(data.clone(), TokenType::Agent))
        .map(endpoint_next_experiment);

    let record_progress = warp::post2()
        .and(warp::path("record-progress"))
        .and(warp::path::end())
        .and(warp::body::json())
        .and(mutex_filter.clone())
        .and(auth_filter(data.clone(), TokenType::Agent))
        .map(endpoint_record_progress);

    let heartbeat = warp::post2()
        .and(warp::path("heartbeat"))
        .and(warp::path::end())
        .and(data_filter)
        .and(auth_filter(data.clone(), TokenType::Agent))
        .map(endpoint_heartbeat);

    let error = warp::post2()
        .and(warp::path("error"))
        .and(warp::path::end())
        .and(warp::body::json())
        .and(mutex_filter)
        .and(auth_filter(data, TokenType::Agent))
        .map(endpoint_error);

    warp::any()
        .and(
            config
                .or(config_old)
                .unify()
                .or(next_experiment)
                .unify()
                .or(record_progress)
                .unify()
                .or(heartbeat)
                .unify()
                .or(error)
                .unify(),
        )
        .map(handle_results)
        .recover(handle_errors)
        .unify()
}

fn endpoint_config(
    caps: Capabilities,
    data: Arc<Data>,
    auth: AuthDetails,
) -> Fallible<Response<Body>> {
    data.agents.add_capabilities(&auth.name, &caps)?;

    Ok(ApiResponse::Success {
        result: AgentConfig {
            agent_name: auth.name,
            crater_config: data.config.clone(),
        },
    }
    .into_response()?)
}

fn endpoint_next_experiment(
    mutex: Arc<Mutex<Data>>,
    github_data: Option<Arc<GithubData>>,
    auth: AuthDetails,
) -> Fallible<Response<Body>> {
    //we need to make sure that Experiment::next executes uninterrupted
    let data = mutex.lock().unwrap();
    let next = Experiment::next(&data.db, &Assignee::Agent(auth.name.clone()))?;
    let result = if let Some((new, ex)) = next {
        if new {
            if let Some(github_data) = github_data.as_ref() {
                if let Some(ref github_issue) = ex.github_issue {
                    Message::new()
                        .line(
                            "construction",
                            format!("Experiment **`{}`** is now **running**", ex.name,),
                        )
                        .send(&github_issue.api_url, &data, github_data)?;
                }
            }
        }

        let running_crates =
            ex.get_running_crates(&data.db, &Assignee::Agent(auth.name.clone()))?;

        //if the agent crashed (i.e. there are already running crates) return those crates
        if !running_crates.is_empty() {
            Some((ex, running_crates))
        } else {
            Some((
                ex.clone(),
                ex.get_uncompleted_crates(&data.db, &data.config, &Assignee::Agent(auth.name))?,
            ))
        }
    } else {
        None
    };

    Ok(ApiResponse::Success { result }.into_response()?)
}

fn endpoint_record_progress(
    result: ExperimentData<ProgressData>,
    mutex: Arc<Mutex<Data>>,
    auth: AuthDetails,
) -> Fallible<Response<Body>> {
    let data = mutex.lock().unwrap();
    let mut ex = Experiment::get(&data.db, &result.experiment_name)?
        .ok_or_else(|| err_msg("no experiment run by this agent"))?;

    data.metrics
        .record_completed_jobs(&auth.name, &ex.name, result.data.results.len() as i64);

    let db = DatabaseDB::new(&data.db);
    db.store(&ex, &result.data, EncodingType::Gzip)?;

    let (completed, all) = ex.raw_progress(&data.db)?;
    if completed == all {
        ex.set_status(&data.db, Status::NeedsReport)?;
        info!("experiment {} completed, marked as needs-report", ex.name);
        data.reports_worker.wake(); // Ensure the reports worker is awake
    }

    Ok(ApiResponse::Success { result: true }.into_response()?)
}

fn endpoint_heartbeat(data: Arc<Data>, auth: AuthDetails) -> Fallible<Response<Body>> {
    if let Some(rev) = auth.git_revision {
        data.agents.set_git_revision(&auth.name, &rev)?;
    }

    data.agents.record_heartbeat(&auth.name)?;
    Ok(ApiResponse::Success { result: true }.into_response()?)
}

fn endpoint_error(
    error: ExperimentData<HashMap<String, String>>,
    mutex: Arc<Mutex<Data>>,
    auth: AuthDetails,
) -> Fallible<Response<Body>> {
    log::error!(
        "agent {} failed while running {}: {:?}",
        auth.name,
        error.experiment_name,
        error.data.get("error")
    );

    let data = mutex.lock().unwrap();
    let mut ex = Experiment::get(&data.db, &error.experiment_name)?
        .ok_or_else(|| err_msg("no experiment run by this agent"))?;

    data.metrics.record_error(&auth.name, &ex.name);
    ex.handle_failure(&data.db, &Assignee::Agent(auth.name))?;

    Ok(ApiResponse::Success { result: true }.into_response()?)
}

fn handle_results(resp: Fallible<Response<Body>>) -> Response<Body> {
    match resp {
        Ok(resp) => resp,
        Err(err) => ApiResponse::internal_error(err.to_string())
            .into_response()
            .unwrap(),
    }
}

fn handle_errors(err: Rejection) -> Result<Response<Body>, Rejection> {
    let error = if let Some(compat) = err.find_cause::<Compat<HttpError>>() {
        Some(*compat.get_ref())
    } else if let StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED = err.status() {
        Some(HttpError::NotFound)
    } else {
        None
    };

    match error {
        Some(HttpError::NotFound) => Ok(ApiResponse::not_found().into_response().unwrap()),
        Some(HttpError::Forbidden) => Ok(ApiResponse::unauthorized().into_response().unwrap()),
        None => Err(err),
    }
}
