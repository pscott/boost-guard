use self::boost_query::BoostQueryBoostStrategyDistribution;
use crate::lottery::cached_lottery_winners;
use crate::routes::boost_query::BoostQueryBoostStrategy;
use crate::routes::boost_query::BoostQueryBoostStrategyEligibility;
use crate::signatures::ClaimConfig;
use crate::State;
use crate::{ServerError, HUB_URL, MYRIAD, SUBGRAPH_URLS};
use ::axum::extract::Json;
use axum::response::IntoResponse;
use axum::Extension;
use cached::proc_macro::cached;
use cached::Cached;
use cached::{SizedCache, TimedSizedCache};
use durations::WEEK;
use ethers::types::Address;
use ethers::types::U256;
use graphql_client::{GraphQLQuery, Response as GraphQLResponse};
use mysql_async::prelude::Queryable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::SystemTime;

pub async fn handle_create_vouchers(
    Extension(state): Extension<State>,
    Json(p): Json<Value>,
) -> Result<impl IntoResponse, ServerError> {
    let reward_infos = get_rewards_inner(&state, p).await?;

    let mut response = Vec::with_capacity(reward_infos.len());
    for reward_info in reward_infos {
        let Ok(claim_cfg) = ClaimConfig::try_from(&reward_info) else {
            continue;
        };
        let Ok(signature) = claim_cfg.create_signature(&state.wallet) else {
            continue;
        };

        response.push(CreateVouchersResponse {
            signature: format!("0x{}", signature),
            reward: reward_info.reward,
            chain_id: reward_info.chain_id,
            boost_id: reward_info.boost_id,
        });
    }
    Ok(Json(response))
}

pub async fn handle_get_rewards(
    Extension(state): Extension<State>,
    Json(p): Json<Value>,
) -> Result<impl IntoResponse, ServerError> {
    let response = get_rewards_inner(&state, p)
        .await?
        .into_iter()
        .map(GetRewardsResponse::from)
        .collect::<Vec<_>>();

    Ok(Json(response))
}

// TODO: kind of a rewrite of get_rewards?
pub async fn handle_get_lottery_winners(
    Extension(state): Extension<State>,
    Json(p): Json<Value>,
) -> Result<impl IntoResponse, ServerError> {
    let request: GetLotteryWinnerQueryParams = serde_json::from_value(p)?;
    let proposal_info: ProposalInfo =
        get_proposal_info(&state.client, &request.proposal_id).await?;

    if let Err(e) = validate_proposal_info(&proposal_info) {
        if let ServerError::ProposalStillInProgress = e {
            // Proposal is still in progress, so we should remove the proposal from the cache.
            let mut cache = GET_PROPOSAL_INFO.lock().await;
            cache.cache_remove(request.proposal_id.as_str());
            return Err(e);
        } else {
            // Proposal is invalid for a reason that will not change with other queries. Just return the error.
            return Err(e);
        }
    }

    let boost_info = get_boost_info(&state.client, &request.boost_id, &request.chain_id).await?;

    // Ensure the requested proposal id actually corresponds to the boosted proposal
    if boost_info.params.proposal != request.proposal_id {
        return Err(ServerError::ErrorString("proposal id mismatch".to_string()));
    }

    if let DistributionType::Lottery(num_winners, limit) = boost_info.params.distribution {
        let winners =
            cached_lottery_winners(&state.pool, &boost_info, &proposal_info, num_winners, limit)
                .await?;

        let response = GetLotteryWinnersResponse {
            winners: winners.keys().map(|a| format!("{a:?}")).collect(),
            prize: winners.values().next().unwrap().to_string(),
            chain_id: request.chain_id.to_string(),
            boost_id: request.boost_id.to_string(),
        };
        Ok(Json(response))
    } else {
        Err(ServerError::ErrorString(
            "boost is not a lottery".to_string(),
        ))
    }
}

pub async fn handle_health() -> Result<impl IntoResponse, ServerError> {
    Ok(axum::response::Html("Healthy!"))
}

// TODO: check with BIG voting power (f64 precision?)
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateVouchersResponse {
    pub signature: String,
    pub reward: String,
    pub chain_id: String,
    pub boost_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetRewardsResponse {
    pub reward: String,
    pub chain_id: String,
    pub boost_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetLotteryWinnersResponse {
    pub winners: Vec<String>,
    pub prize: String,
    pub chain_id: String,
    pub boost_id: String,
}

impl From<RewardInfo> for GetRewardsResponse {
    fn from(reward_info: RewardInfo) -> Self {
        Self {
            reward: reward_info.reward,
            chain_id: reward_info.chain_id,
            boost_id: reward_info.boost_id,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RewardInfo {
    pub voter_address: String,
    pub reward: String,
    pub chain_id: String,
    pub boost_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct QueryParams {
    pub proposal_id: String,
    pub voter_address: String,
    pub boosts: Vec<(String, String)>, // Vec<(boost_id, chain_id)>
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetLotteryWinnerQueryParams {
    pub proposal_id: String,
    pub boost_id: String,
    pub chain_id: String,
}

type Bytes = Address;
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql/subgraph_schema.json",
    query_path = "src/graphql/boost_query.graphql",
    response_derives = "Debug"
)]
struct BoostQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql/hub_schema.graphql",
    query_path = "src/graphql/proposal_query.graphql",
    response_derives = "Debug"
)]
struct ProposalQuery;

// List of different types of strategies supported
#[derive(Debug, Default)]
pub enum BoostStrategy {
    #[default]
    Proposal, // Boost a specific proposal
}

impl TryFrom<&str> for BoostStrategy {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "proposal" => Ok(BoostStrategy::Proposal),
            _ => Err("Invalid strategy"),
        }
    }
}

#[allow(dead_code)] // needed for `strategy` field
#[derive(Debug, Default)]
pub struct BoostInfo {
    pub id: u64,
    pub chain_id: U256,
    pub strategy: BoostStrategy,
    pub params: BoostParams,
    pub pool_size: U256,
    pub decimals: u8,
}

impl TryFrom<(boost_query::BoostQueryBoost, &str)> for BoostInfo {
    type Error = &'static str;

    fn try_from(value: (boost_query::BoostQueryBoost, &str)) -> Result<Self, Self::Error> {
        let id = value.0.id.parse().map_err(|_| "failed to parse id")?;
        let chain_id = U256::from_dec_str(value.1).map_err(|_| "failed to parse chain id")?;
        let strategy: BoostQueryBoostStrategy =
            value.0.strategy.ok_or("strategy missing from query")?;
        let name = strategy.name.as_str();
        let strategy_type = BoostStrategy::try_from(name)?;

        match strategy_type {
            BoostStrategy::Proposal => {
                let eligibility = BoostEligibility::try_from(strategy.eligibility)?;

                let distribution = DistributionType::try_from(strategy.distribution)?;

                let bp = BoostParams {
                    version: strategy.version,
                    proposal: strategy.proposal,
                    eligibility,
                    distribution,
                };

                let pool_size = U256::from_dec_str(&value.0.pool_size)
                    .map_err(|_| "failed to parse pool size")?;
                let decimals = value
                    .0
                    .token
                    .decimals
                    .parse()
                    .map_err(|_| "failed to parse decimals")?;

                Ok(Self {
                    id,
                    chain_id,
                    strategy: strategy_type,
                    params: bp,
                    pool_size,
                    decimals,
                })
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct BoostParams {
    pub version: String,
    pub proposal: String,
    pub eligibility: BoostEligibility,
    pub distribution: DistributionType,
}

#[derive(Debug, Copy, Clone, Default)]
pub enum BoostEligibility {
    #[default]
    Incentive, // Everyone who votes is eligible, regardless of choice
    Bribe(usize), // Only those who voted for the specific choice are eligible
}

impl BoostEligibility {
    pub fn boosted_choice(&self) -> Option<usize> {
        if let BoostEligibility::Bribe(choice) = self {
            Some(*choice)
        } else {
            None
        }
    }
}

impl TryFrom<BoostQueryBoostStrategyEligibility> for BoostEligibility {
    type Error = &'static str;

    fn try_from(value: BoostQueryBoostStrategyEligibility) -> Result<Self, Self::Error> {
        match value.type_.as_str() {
            "incentive" => Ok(BoostEligibility::Incentive),
            "bribe" => {
                let choice = value
                    .choice
                    .ok_or("missing choice")?
                    .parse()
                    .map_err(|_| "failed to parse choice")?;
                if choice == 0 {
                    return Err("invalid choice: 0");
                }
                Ok(BoostEligibility::Bribe(choice))
            }
            _ => Err("invalid eligibility"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DistributionType {
    Weighted(Option<U256>), // The option represents the maximum amount of tokens that can be rewarded. If None, there is no limit.
    Even,
    Lottery(u32, Option<u16>), // The number of winners
}

impl Default for DistributionType {
    fn default() -> Self {
        DistributionType::Weighted(None)
    }
}

impl TryFrom<boost_query::BoostQueryBoostStrategyDistribution> for DistributionType {
    type Error = &'static str;

    fn try_from(value: BoostQueryBoostStrategyDistribution) -> Result<Self, Self::Error> {
        match value.type_.as_str() {
            "weighted" => {
                if let Some(limit) = value.limit {
                    match U256::from_dec_str(&limit) {
                        Ok(limit) => Ok(DistributionType::Weighted(Some(limit))),
                        Err(_) => Err("failed to parse limit"),
                    }
                } else {
                    Ok(DistributionType::Weighted(None))
                }
            }
            "even" => Ok(DistributionType::Even),
            "lottery" => {
                let num_winners = value
                    .num_winners
                    .ok_or("missing num winners")?
                    .parse()
                    .map_err(|_| "failed to parse num winners")?;

                let limit = value.limit;
                if let Some(l) = limit {
                    match l.parse() {
                        Ok(l) => {
                            if l > MYRIAD {
                                Err("limit is too high")
                            } else {
                                Ok(DistributionType::Lottery(num_winners, Some(l)))
                            }
                        }
                        Err(_) => Err("failed to parse limit"),
                    }
                } else {
                    Ok(DistributionType::Lottery(num_winners, None))
                }
            }
            _ => Err("invalid distribution"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Vote {
    pub voter: Address,
    pub voting_power: f64,
}

#[derive(Debug, Clone)]
pub struct VoteWithChoice {
    pub voter: Address,
    pub voting_power: f64,
    pub choice: usize,
}

impl Default for Vote {
    fn default() -> Self {
        Self {
            voter: Address::random(),
            voting_power: 1.0,
        }
    }
}

impl Default for VoteWithChoice {
    fn default() -> Self {
        Self {
            voter: Address::random(),
            voting_power: 1.0,
            choice: 1,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProposalInfo {
    pub id: String,
    pub type_: String,
    pub score: f64,
    pub scores_by_choice: Vec<f64>,
    pub end: u64,
    pub num_votes: u64,
}

impl ProposalInfo {
    fn get_score(&self, eligibility: BoostEligibility) -> f64 {
        if let Some(choice) = eligibility.boosted_choice() {
            self.scores_by_choice[choice - 1]
        } else {
            self.score
        }
    }
}

impl TryFrom<proposal_query::ProposalQueryProposal> for ProposalInfo {
    type Error = ServerError;

    fn try_from(proposal: proposal_query::ProposalQueryProposal) -> Result<Self, Self::Error> {
        let id = proposal.id;
        let type_ = proposal.type_.ok_or("missing proposal type from the hub")?;
        let scores_by_choice = proposal
            .scores
            .ok_or("missing proposal scores from the hub")?
            .into_iter()
            .map(|choice| choice.ok_or("missing choice in scores by choices"))
            .collect::<Result<Vec<_>, _>>()?;
        let score = proposal
            .scores_total
            .ok_or("missing proposal scores_total from the hub")?;
        let end = proposal.end.try_into()?;
        let num_votes = proposal
            .votes
            .ok_or("proposal: missing votes from the hub")?
            .try_into()
            .map_err(|_| ServerError::ErrorString("failed to parse votes".to_string()))?;

        Ok(ProposalInfo {
            id,
            type_,
            score,
            scores_by_choice,
            end,
            num_votes,
        })
    }
}

// Helper function to compute the rewards for a given boost and a user request
async fn get_rewards_inner(
    state: &State,
    p: serde_json::Value,
) -> Result<Vec<RewardInfo>, ServerError> {
    let request: QueryParams = serde_json::from_value(p)?;

    let proposal_info: ProposalInfo =
        get_proposal_info(&state.client, &request.proposal_id).await?;

    if let Err(e) = validate_proposal_info(&proposal_info) {
        if let ServerError::ProposalStillInProgress = e {
            // Proposal is still in progress, so we should remove the proposal from the cache.
            let mut cache = GET_PROPOSAL_INFO.lock().await;
            cache.cache_remove(request.proposal_id.as_str());
            return Err(e);
        } else {
            // Proposal is invalid for a reason that will not change with other queries. Just return the error.
            return Err(e);
        }
    }

    let vote_info =
        get_vote_info(&state.pool, &request.voter_address, &request.proposal_id).await?;

    let mut response = Vec::with_capacity(request.boosts.len());
    for (boost_id, chain_id) in request.boosts {
        let boost_info = match get_boost_info(&state.client, &boost_id, &chain_id).await {
            Ok(boost_info) => boost_info,
            Err(e) => {
                eprintln!("{:?}", e);
                continue;
            }
        };

        // Ensure the requested proposal id actually corresponds to the boosted proposal
        if boost_info.params.proposal != request.proposal_id {
            eprintln!("proposal id mismatch");
            continue;
        }

        match validate_choice(vote_info.choice, boost_info.params.eligibility) {
            Ok(_) => (),
            Err(e) => {
                eprintln!("{:?}", e);
                continue;
            }
        }

        let reward =
            match get_user_reward(&state.pool, &boost_info, &proposal_info, &vote_info).await {
                Ok(reward) => reward,
                Err(e) => {
                    eprintln!("{:?}", e);
                    continue;
                }
            };

        response.push(RewardInfo {
            voter_address: request.voter_address.clone(),
            reward: reward.to_string(),
            chain_id,
            boost_id,
        });
    }

    Ok(response)
}

#[cached(
    result = true,
    sync_writes = true,
    type = "TimedSizedCache<String, ProposalInfo>",
    create = "{ TimedSizedCache::with_size_and_lifespan(100, 3 * WEEK.as_secs()) }",
    convert = r#"{ proposal_id.to_string() }"#
)]
async fn get_proposal_info(
    client: &reqwest::Client,
    proposal_id: &str,
) -> Result<ProposalInfo, ServerError> {
    let variables = proposal_query::Variables {
        id: proposal_id.to_owned(),
    };

    let request_body = ProposalQuery::build_query(variables);

    let res = client
        .post(HUB_URL.as_str())
        .json(&request_body)
        .send()
        .await?;
    let response_body: GraphQLResponse<proposal_query::ResponseData> = res.json().await?;
    let proposal_query: proposal_query::ProposalQueryProposal = response_body
        .data
        .ok_or("missing data from the hub")?
        .proposal
        .ok_or("missing proposal data from the hub")?;
    ProposalInfo::try_from(proposal_query)
}

async fn get_boost_info(
    client: &reqwest::Client,
    boost_id: &str,
    chain_id: &str,
) -> Result<BoostInfo, ServerError> {
    let variables = boost_query::Variables {
        id: boost_id.to_owned(),
    };

    let request_body = BoostQuery::build_query(variables);

    let res = client
        .post(SUBGRAPH_URLS.get(chain_id).unwrap().as_str())
        .json(&request_body)
        .send()
        .await?;
    let response_body: GraphQLResponse<boost_query::ResponseData> = res.json().await?;
    let boost_query = response_body.data.ok_or("missing data from the graph")?;

    let boost = boost_query.boost.ok_or("missing boost from the graph")?;
    Ok(BoostInfo::try_from((boost, chain_id))?)
}

#[cached(
    result = true,
    sync_writes = true,
    type = "TimedSizedCache<String, VoteWithChoice>",
    create = "{ TimedSizedCache::with_size_and_lifespan(2000, 3 * WEEK.as_secs()) }",
    convert = r#"{ format!("{}{}", voter_address, proposal_id) }"#
)]
async fn get_vote_info(
    pool: &mysql_async::Pool,
    voter_address: &str,
    proposal_id: &str,
) -> Result<VoteWithChoice, ServerError> {
    let mut conn = pool.get_conn().await?;

    let query = format!(
        "SELECT voter, vp, choice
        FROM votes
        WHERE proposal = '{}'
        AND voter = '{}'
        ORDER BY vp DESC;",
        proposal_id, voter_address
    );
    println!("QUERY: {}", query);

    let (_voter, voting_power, choice): (String, f64, usize) = conn
        .query_first(query)
        .await?
        .ok_or("could not find vote for voter and proposal in the database")?;

    conn.disconnect().await?;

    Ok(VoteWithChoice {
        voter: Address::from_str(voter_address)?,
        voting_power,
        choice,
    })
}

async fn get_user_reward(
    pool: &mysql_async::Pool,
    boost_info: &BoostInfo,
    proposal_info: &ProposalInfo,
    vote_info: &VoteWithChoice,
) -> Result<U256, ServerError> {
    match &boost_info.params.distribution {
        DistributionType::Even => {
            println!("EVEN");
            if let Some(boosted_choice) = boost_info.params.eligibility.boosted_choice() {
                println!("BOOSTED {}", boosted_choice);
                // Only count the number of votes that voted for the boosted choice
                let num_votes = cached_num_votes(pool, boost_info, proposal_info, boosted_choice);
                Ok(boost_info.pool_size / num_votes.await?)
            } else {
                println!("Not boosted");
                Ok(boost_info.pool_size / (U256::from(proposal_info.num_votes)))
            }
        }
        DistributionType::Weighted(l) => {
            if let Some(limit) = l {
                let rewards =
                    cached_weighted_rewards(pool, boost_info, proposal_info, *limit).await?;
                Ok(*rewards
                    .get(&vote_info.voter)
                    .expect("voter should appear in hashmap"))
            } else {
                let pow = cached_pow(boost_info.decimals);
                let score = U256::from(
                    (proposal_info.get_score(boost_info.params.eligibility) * pow) as u128,
                );
                let voting_power = U256::from((vote_info.voting_power * pow) as u128);
                Ok((voting_power * boost_info.pool_size) / score)
            }
        }
        DistributionType::Lottery(num_winners, limit) => {
            let winners =
                cached_lottery_winners(pool, boost_info, proposal_info, *num_winners, *limit)
                    .await?;
            Ok(*winners
                .get(&vote_info.voter)
                .ok_or("voter did not win this time!")?)
        }
    }
}

// LRU cache that uses `boost_id` and `chain_id` as keys
#[cached(
    result = true,
    sync_writes = true,
    type = "TimedSizedCache<String, u32>",
    create = "{ TimedSizedCache::with_size_and_lifespan(500, 3 * WEEK.as_secs()) }",
    convert = r#"{ format!("{}{}", _boost_info.id, _boost_info.chain_id) }"#
)]
async fn cached_num_votes(
    pool: &mysql_async::Pool,
    _boost_info: &BoostInfo,
    proposal_info: &ProposalInfo,
    boosted_choice: usize,
) -> Result<u32, ServerError> {
    let query = format!(
        "
        SELECT COUNT(*) AS total_votes
        FROM votes
        WHERE proposal = '{}'
        AND choice = {};",
        proposal_info.id, boosted_choice,
    );

    let mut conn = pool.get_conn().await?;
    let (num_votes,): (i64,) = conn
        .query_first(query)
        .await?
        .ok_or("failed to fetch number of votes from db")?;

    conn.disconnect().await?;

    Ok(num_votes as u32)
}

#[cached(
    sync_writes = true,
    type = "SizedCache<u8, f64>",
    create = "{ SizedCache::with_size(18) }"
)]
fn cached_pow(decimals: u8) -> f64 {
    10f64.powi(decimals as i32)
}

// LRU cache that uses `boost_id` and `chain_id` as keys
#[cached(
    result = true,
    sync_writes = true,
    type = "TimedSizedCache<String, HashMap<Address, U256>>",
    create = "{ TimedSizedCache::with_size_and_lifespan(100, 3 * WEEK.as_secs()) }",
    convert = r#"{ format!("{}{}", boost_info.id, boost_info.chain_id) }"#
)]
async fn cached_weighted_rewards(
    pool: &mysql_async::Pool,
    boost_info: &BoostInfo,
    proposal_info: &ProposalInfo,
    limit: U256,
) -> Result<HashMap<Address, U256>, ServerError> {
    let mut conn = pool.get_conn().await?;

    let choice_clause = if let BoostEligibility::Bribe(choice) = boost_info.params.eligibility {
        format!("AND choice = {}", choice)
    } else {
        "".to_string()
    };
    println!("CHOICE {}", choice_clause);

    let query = format!(
        "SELECT voter, vp
        FROM votes
        WHERE proposal = '{}'
        {}
        ORDER BY vp DESC;",
        proposal_info.id, choice_clause
    );

    let votes: Vec<Vote> = conn
        .query_map(query, |(voter, vp): (String, f64)| {
            let v = Address::from_str(voter.as_str()).expect("address is ill-formatted");

            Vote {
                voter: v,
                voting_power: vp,
            }
        })
        .await?;

    compute_rewards(
        votes,
        boost_info.pool_size,
        boost_info.decimals,
        proposal_info.get_score(boost_info.params.eligibility),
        limit,
    )
}

fn compute_rewards(
    votes: Vec<Vote>,
    mut pool_size: U256,
    decimals: u8,
    _score_decimal: f64,
    limit: U256,
) -> Result<HashMap<Address, U256>, ServerError> {
    let pow = cached_pow(decimals);

    // Ensure the vector is sorted
    if votes
        .windows(2)
        .any(|w| w[0].voting_power < w[1].voting_power)
    {
        return Err(ServerError::ErrorString("votes are not sorted".to_string()));
    }

    // let mut score = U256::from((score_decimal * pow) as u128); // TODO: investigate why this value is incorrect
    let mut score = votes.iter().fold(U256::from(0), |acc, vote_info| {
        acc + U256::from((vote_info.voting_power * pow) as u128)
    });

    // TODO: optimize: we could check if the first voter reaches limit. If he doesn't, then we can simplify the computation.

    Ok(votes
        .into_iter()
        .map(|vote_info| {
            let vp = U256::from((vote_info.voting_power * pow) as u128);
            let reward = vp * pool_size / score;
            let actual_reward = std::cmp::min(reward, limit);

            pool_size -= actual_reward;
            score -= vp;

            (vote_info.voter, actual_reward)
        })
        .collect())
}

fn validate_proposal_info(proposal_info: &ProposalInfo) -> Result<(), ServerError> {
    validate_end_time(proposal_info.end)?;
    validate_type(&proposal_info.type_)?;
    Ok(())
}

// We don't need to validate start_time because the smart-contract will do it anyway.
fn validate_end_time(end: u64) -> Result<(), ServerError> {
    let current_timestamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap() // Safe to unwrap because we are sure that the current time is after the UNIX_EPOCH
        .as_secs();
    if current_timestamp < end {
        Err(ServerError::ProposalStillInProgress)
    } else {
        Ok(())
    }
}

// Only single-choice and basic proposals are eligible for boosting.
// The other types are not supported yet (and not for the near future).
fn validate_type(type_: &str) -> Result<(), ServerError> {
    if (type_ != "single-choice") && (type_ != "basic") {
        Err(ServerError::ErrorString(format!(
            "`{type_:}` proposals are not eligible for boosting"
        )))
    } else {
        Ok(())
    }
}

fn validate_choice(choice: usize, boost_eligibility: BoostEligibility) -> Result<(), ServerError> {
    match boost_eligibility {
        BoostEligibility::Incentive => Ok(()),
        BoostEligibility::Bribe(boosted_choice) => {
            if choice != boosted_choice {
                Err(ServerError::ErrorString(format!(
                    "voter voted {:} but needed to vote {} to be eligible",
                    choice, boosted_choice
                )))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "expensive_tests")]
mod test_cached_results {
    use crate::routes::get_proposal_info;

    use super::*;
    use super::{CACHED_NUM_VOTES, CACHED_WEIGHTED_REWARDS};
    use cached::Cached;
    use dotenv::dotenv;
    use ethers::types::{Address, U256};
    use mysql_async::Pool;
    use std::str::FromStr;

    const ELIGIBLE_VOTERS: usize = 210_613;

    #[tokio::test]
    async fn test_num_voters() {
        dotenv().ok();

        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = Pool::new(database_url.as_str());
        let boost_info = Default::default();
        let client = reqwest::Client::new();
        let proposal_id = "0x11e9daab4e806cba220d5d6eae6be76f799f27ad20723d0aabedf0263ca2a28f";
        let proposal_info = get_proposal_info(&client, proposal_id).await.unwrap();
        let boosted_choice = 1;

        let num_votes = cached_num_votes(&pool, &boost_info, &proposal_info, boosted_choice)
            .await
            .unwrap();

        assert_eq!(num_votes as usize, ELIGIBLE_VOTERS);

        assert!(CACHED_NUM_VOTES.lock().await.cache_hits() == Some(0));
        let _ = cached_num_votes(&pool, &boost_info, &proposal_info, boosted_choice)
            .await
            .unwrap();
        assert!(CACHED_NUM_VOTES.lock().await.cache_hits() == Some(1));
    }

    #[tokio::test]
    async fn test_cached_weighted_rewards_bribed() {
        dotenv().ok();

        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = Pool::new(database_url.as_str());
        let limit = U256::from(10000000000000000000000_u128);
        let proposal_id = "0x11e9daab4e806cba220d5d6eae6be76f799f27ad20723d0aabedf0263ca2a28f";
        let boost_info = BoostInfo {
            id: 1,
            chain_id: U256::from(11155111),
            strategy: BoostStrategy::Proposal,
            params: BoostParams {
                version: "1".to_string(),
                proposal: proposal_id.to_string(),
                eligibility: BoostEligibility::Bribe(1),
                distribution: DistributionType::Weighted(Some(limit)),
            },
            pool_size: U256::from(10000000000000000000000_u128), // 10_000 * 10**18
            decimals: 18,
        };
        let client = reqwest::Client::new();
        let proposal_info = get_proposal_info(&client, proposal_id).await.unwrap();

        let rewards = cached_weighted_rewards(&pool, &boost_info, &proposal_info, limit)
            .await
            .unwrap();

        // Ensure all reward is distributed
        let sum: U256 = rewards.values().fold(U256::from(0), |acc, x| acc + x);
        assert_eq!(sum, boost_info.pool_size);

        // Ensure all eligible voters are rewarded
        assert_eq!(rewards.len(), ELIGIBLE_VOTERS);

        // Pick three values at random
        assert_eq!(
            *rewards
                .get(&Address::from_str("0x0E457324f0c6125b20392341Cdeb7bf9bCB02322").unwrap())
                .unwrap(),
            U256::from(210367026718988690605_u128)
        );
        // User who voted No does not get rewarded
        assert_eq!(
            rewards.get(&Address::from_str("0xB9cF551E73bEC54332D76A7542FdacBb77BFA430").unwrap()),
            None
        );
        assert_eq!(
            *rewards
                .get(&Address::from_str("0x31B6BE9b49974A66F1A2C3787B44E694AD13EC27").unwrap())
                .unwrap(),
            U256::from(14128175333414183359_u128)
        );

        let hits = CACHED_WEIGHTED_REWARDS.lock().await.cache_hits().unwrap();
        let _ = cached_weighted_rewards(&pool, &boost_info, &proposal_info, limit)
            .await
            .unwrap();
        assert!(CACHED_WEIGHTED_REWARDS.lock().await.cache_hits() == Some(hits + 1));

        // -------
        // Now, a new boost that will reach the limit
        // -------

        let limit = U256::from(84466625025568633775_u128);
        let proposal_id = "0x11e9daab4e806cba220d5d6eae6be76f799f27ad20723d0aabedf0263ca2a28f";
        let boost_info = BoostInfo {
            id: 2,
            chain_id: U256::from(11155111),
            strategy: BoostStrategy::Proposal,
            params: BoostParams {
                version: "1".to_string(),
                proposal: proposal_id.to_string(),
                eligibility: BoostEligibility::Bribe(1),
                distribution: DistributionType::Weighted(Some(limit)),
            },
            pool_size: U256::from(10000000000000000000000_u128), // 10_000 * 10**18
            decimals: 18,
        };

        let rewards = cached_weighted_rewards(&pool, &boost_info, &proposal_info, limit)
            .await
            .unwrap();

        // Ensure all reward is distributed
        let sum: U256 = rewards.values().fold(U256::from(0), |acc, x| acc + x);
        assert_eq!(sum, boost_info.pool_size);

        // Ensure the biggest voter reaches the limit
        assert_eq!(
            *rewards
                .get(&Address::from_str("0xe0dEDCDb5B5Ef2c82E4AdC60AACC23486A518357").unwrap())
                .unwrap(),
            limit
        );

        // Other voters should have a different reward
        assert_eq!(
            *rewards
                .get(&Address::from_str("0x0E457324f0c6125b20392341Cdeb7bf9bCB02322").unwrap())
                .unwrap(),
            U256::from(84466625025568633775_u128)
        );
        // User who voted No does not get rewarded
        assert_eq!(
            rewards.get(&Address::from_str("0xB9cF551E73bEC54332D76A7542FdacBb77BFA430").unwrap()),
            None
        );
        assert_eq!(
            *rewards
                .get(&Address::from_str("0x31B6BE9b49974A66F1A2C3787B44E694AD13EC27").unwrap())
                .unwrap(),
            U256::from(15514329941501828111_u128)
        );
    }

    #[tokio::test]
    async fn test_cached_weighted_rewards_incentivized() {
        dotenv().ok();

        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = Pool::new(database_url.as_str());
        let limit = U256::from(384012049357245359479_u128); // 394012049357245359479 is the reward for the first voter, with no limit. We simply go from 39 to 38.
        let proposal_id = "0x11e9daab4e806cba220d5d6eae6be76f799f27ad20723d0aabedf0263ca2a28f";
        let boost_info = BoostInfo {
            id: 3,
            chain_id: U256::from(11155111),
            strategy: BoostStrategy::Proposal,
            params: BoostParams {
                version: "1".to_string(),
                proposal: proposal_id.to_string(),
                eligibility: BoostEligibility::Incentive,
                distribution: DistributionType::Weighted(Some(limit)),
            },
            pool_size: U256::from(10000000000000000000000_u128), // 10_000 * 10**18
            decimals: 18,
        };
        let client = reqwest::Client::new();
        let proposal_info = get_proposal_info(&client, proposal_id).await.unwrap();

        let rewards = cached_weighted_rewards(&pool, &boost_info, &proposal_info, limit)
            .await
            .unwrap();

        // Ensure all eligible voters are rewarded
        assert_eq!(rewards.len(), 237_573); // all voters are eligible

        // Ensure all reward is distributed
        let sum: U256 = rewards.values().fold(U256::from(0), |acc, x| acc + x);
        assert_eq!(sum, boost_info.pool_size);

        // Ensure first voter hits the reward limit
        assert_eq!(
            *rewards
                .get(&Address::from_str("0xe0dEDCDb5B5Ef2c82E4AdC60AACC23486A518357").unwrap())
                .unwrap(),
            limit,
        );

        // Pick three values at random
        assert_eq!(
            *rewards
                .get(&Address::from_str("0x0E457324f0c6125b20392341Cdeb7bf9bCB02322").unwrap())
                .unwrap(),
            U256::from(196464414774155419006_u128)
        );
        // User who voted no still gets rewarded
        assert_eq!(
            *rewards
                .get(&Address::from_str("0xB9cF551E73bEC54332D76A7542FdacBb77BFA430").unwrap())
                .unwrap(),
            U256::from(27718149939250144849_u128)
        );
        assert_eq!(
            *rewards
                .get(&Address::from_str("0x31B6BE9b49974A66F1A2C3787B44E694AD13EC27").unwrap())
                .unwrap(),
            U256::from(13194480817631529200_u128)
        );

        let hits = CACHED_WEIGHTED_REWARDS.lock().await.cache_hits().unwrap();
        let _ = cached_weighted_rewards(&pool, &boost_info, &proposal_info, limit)
            .await
            .unwrap();
        assert!(CACHED_WEIGHTED_REWARDS.lock().await.cache_hits() == Some(hits + 1));
    }
}

#[cfg(test)]
mod test_compute_rewards {
    use crate::ServerError;

    use super::{compute_rewards, Vote};
    use ethers::types::{Address, U256};

    #[test]
    fn test_compute_rewards_unsorted() {
        let decimals = 18u8;
        let pow = 10f64.powi(decimals as i32);
        let user1 = Vote {
            voting_power: 1.0,
            ..Default::default()
        };
        let user2 = Vote {
            voting_power: 3.0,
            ..Default::default()
        };
        let user3 = Vote {
            voting_power: 2.0,
            ..Default::default()
        };
        let query_results = vec![user1, user2, user3];

        let score_decimal = 100.0;
        let pool_decimal = 200.0;
        let pool = U256::from((pool_decimal * pow) as u128);
        let reward_limit_decimal = 110.0;
        let limit = U256::from((reward_limit_decimal * pow) as u128);

        let rewards = compute_rewards(query_results, pool, decimals, score_decimal, limit);
        assert_eq!(
            rewards.unwrap_err(),
            ServerError::ErrorString("votes are not sorted".to_string())
        );
    }

    #[test]
    fn test_compute_rewards_empty() {
        let decimals = 18u8;
        let pow = 10f64.powi(decimals as i32);
        let query_results = vec![];

        let score_decimal = 100.0;
        let pool_decimal = 200.0;
        let pool = U256::from((pool_decimal * pow) as u128);
        let reward_limit_decimal = 110.0;
        let limit = U256::from((reward_limit_decimal * pow) as u128);

        let rewards = compute_rewards(query_results, pool, decimals, score_decimal, limit).unwrap();
        assert!(rewards.is_empty());
    }

    #[test]
    fn test_compute_rewards_single() {
        let decimals = 18u8;
        let pow = 10f64.powi(decimals as i32);
        let user1 = Vote {
            voter: Address::random(),
            voting_power: 91.0,
        };
        let query_results = vec![user1.clone()];

        let score_decimal = 100.0;
        let pool_decimal = 200.0;
        let pool = U256::from((pool_decimal * pow) as u128);
        let reward_limit_decimal = 110.0;
        let limit = U256::from((reward_limit_decimal * pow) as u128);

        let rewards = compute_rewards(query_results, pool, decimals, score_decimal, limit).unwrap();
        assert!(rewards.len() == 1);
        assert_eq!(*rewards.get(&user1.voter).unwrap(), limit);
    }

    #[test]
    fn test_compute_rewards_six() {
        // user1: 38.5 vp
        // user2: 25 vp
        // user3: 20 vp
        // user4: 15 vp
        // user5: 1 vp
        // user6: 0.5 vp
        //
        // limit: 10
        // pool: 200
        // score: 100
        //
        // rewards:
        // user1: 38.5 * 200 / 100 > limit => 10
        // user2: 25 * 190 / 61.5 > limit => 10
        // user3: 20 * 180 / 36.5 > limit => 10
        // user4: 15 * 170 / 16.5 > limit => 10
        // user5: 1 * 160 / 1.5 > limit => 10
        // user6: 0.5 * 150 / 0.5 > limit => 10
        let user1 = Vote {
            voter: Address::random(),
            voting_power: 38.5,
        };
        let user2 = Vote {
            voter: Address::random(),
            voting_power: 25.0,
        };
        let user3 = Vote {
            voter: Address::random(),
            voting_power: 20.0,
        };
        let user4 = Vote {
            voter: Address::random(),
            voting_power: 15.0,
        };
        let user5 = Vote {
            voter: Address::random(),
            voting_power: 1.0,
        };
        let user6 = Vote {
            voter: Address::random(),
            voting_power: 0.5,
        };
        let decimals = 18u8;
        let pow = 10f64.powi(decimals as i32);
        let query_results = vec![
            user1.clone(),
            user2.clone(),
            user3.clone(),
            user4.clone(),
            user5.clone(),
            user6.clone(),
        ];

        let score_decimal = 100.0;
        let pool_decimal = 200.0;
        let pool_size = U256::from((pool_decimal * pow) as u128);
        let reward_limit_decimal = 10.0;
        let limit = U256::from((reward_limit_decimal * pow) as u128);

        let rewards =
            compute_rewards(query_results, pool_size, decimals, score_decimal, limit).unwrap();

        assert_eq!(*rewards.get(&user1.voter).unwrap(), limit);
        assert_eq!(*rewards.get(&user2.voter).unwrap(), limit);
        assert_eq!(*rewards.get(&user3.voter).unwrap(), limit);
        assert_eq!(*rewards.get(&user4.voter).unwrap(), limit);
        assert_eq!(*rewards.get(&user5.voter).unwrap(), limit,);
    }

    #[test]
    fn test_compute_small_user() {
        // user1: 90 vp
        // user2: 9 vp
        // user3: 1 vp
        //
        // limit: 40
        // pool: 200
        let user1 = Vote {
            voter: Address::random(),
            voting_power: 90.0,
        };
        let user2 = Vote {
            voter: Address::random(),
            voting_power: 9.0,
        };
        let user3 = Vote {
            voter: Address::random(),
            voting_power: 1.0,
        };

        let decimals = 18u8;
        let pow = 10f64.powi(decimals as i32);
        let query_results = vec![user1.clone(), user2.clone(), user3.clone()];

        let score_decimal = 100.0;
        let pool_size_decimal = 200.0;
        let pool_size = U256::from((pool_size_decimal * pow) as u128);
        let reward_limit_decimal = 40.0;
        let limit = U256::from((reward_limit_decimal * pow) as u128);

        let rewards =
            compute_rewards(query_results, pool_size, decimals, score_decimal, limit).unwrap();
        assert_eq!(*rewards.get(&user1.voter).unwrap(), limit);
        assert_eq!(*rewards.get(&user2.voter).unwrap(), limit);
        assert_eq!(*rewards.get(&user3.voter).unwrap(), limit);
    }
}

#[cfg(test)]
mod test_compute_user_reward {
    use super::BoostParams;
    use super::{get_user_reward, DistributionType};
    use super::{BoostInfo, ProposalInfo, VoteWithChoice};
    use ethers::types::U256;

    #[tokio::test]
    async fn even_distribution_one_voter() {
        let voting_power = 10.0;
        let proposal_score = U256::from(100);
        let pool_size = U256::from(100);
        let num_votes = 1;
        let boost_info: BoostInfo = BoostInfo {
            pool_size,
            params: BoostParams {
                distribution: DistributionType::Even,
                ..Default::default()
            },
            ..Default::default()
        };
        let proposal_info = ProposalInfo {
            score: proposal_score.as_u128() as f64,
            num_votes,
            ..Default::default()
        };
        let vote_info = VoteWithChoice {
            voting_power,
            ..Default::default()
        };
        let pool = mysql_async::Pool::new("mysql://username:password@toto:3306/db");

        let reward = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info)
            .await
            .unwrap();

        assert_eq!(reward, pool_size);
    }

    #[tokio::test]
    async fn even_distribution_two_voters() {
        let proposal_score = U256::from(100);
        let pool_size = U256::from(100);
        let num_votes = 2;
        let boost_info: BoostInfo = BoostInfo {
            pool_size,
            params: BoostParams {
                distribution: DistributionType::Even,
                ..Default::default()
            },
            ..Default::default()
        };
        let proposal_info = ProposalInfo {
            score: proposal_score.as_u128() as f64,
            num_votes,
            ..Default::default()
        };

        let voting_power1 = 10.0;
        let voting_power2 = 20.0;

        let vote_info1 = VoteWithChoice {
            voting_power: voting_power1,
            ..Default::default()
        };
        let vote_info2 = VoteWithChoice {
            voting_power: voting_power2,
            ..Default::default()
        };

        let pool = mysql_async::Pool::new("mysql://username:password@toto:3306/db");

        let reward1 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info1)
            .await
            .unwrap();
        let reward2 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info2)
            .await
            .unwrap();

        assert_eq!(reward2, reward1);
        assert_eq!(reward1, pool_size / 2);
    }

    #[tokio::test]
    async fn even_distribution_three_voters() {
        let proposal_score = U256::from(100);
        let pool_size = U256::from(100);
        let num_votes = 3;
        let boost_info: BoostInfo = BoostInfo {
            pool_size,
            params: BoostParams {
                distribution: DistributionType::Even,
                ..Default::default()
            },
            ..Default::default()
        };
        let proposal_info = ProposalInfo {
            score: proposal_score.as_u128() as f64,
            num_votes,
            ..Default::default()
        };

        let voting_power1 = 10.0;
        let voting_power2 = 20.0;
        let voting_power3 = 30.0;

        let vote_info1 = VoteWithChoice {
            voting_power: voting_power1,
            ..Default::default()
        };
        let vote_info2 = VoteWithChoice {
            voting_power: voting_power2,
            ..Default::default()
        };
        let vote_info3 = VoteWithChoice {
            voting_power: voting_power3,
            ..Default::default()
        };

        let pool = mysql_async::Pool::new("mysql://username:password@toto:3306/db"); // random

        let reward1 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info1)
            .await
            .unwrap();
        let reward2 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info2)
            .await
            .unwrap();
        let reward3 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info3)
            .await
            .unwrap();

        assert_eq!(reward1, reward2);
        assert_eq!(reward2, reward3);
        assert_eq!(reward1, pool_size / 3);
    }

    #[tokio::test]
    async fn weighted_distribution_three_voters() {
        let proposal_score = U256::from(100);
        let pool_size = U256::from(100);
        let num_votes = 3;
        let boost_info: BoostInfo = BoostInfo {
            pool_size,
            params: BoostParams {
                distribution: DistributionType::Weighted(None),
                ..Default::default()
            },
            ..Default::default()
        };
        let proposal_info = ProposalInfo {
            score: proposal_score.as_u128() as f64,
            num_votes,
            ..Default::default()
        };

        let voting_power1 = 10.0;
        let voting_power2 = 20.0;
        let voting_power3 = 30.0;

        let vote_info1 = VoteWithChoice {
            voting_power: voting_power1,
            ..Default::default()
        };
        let vote_info2 = VoteWithChoice {
            voting_power: voting_power2,
            ..Default::default()
        };
        let vote_info3 = VoteWithChoice {
            voting_power: voting_power3,
            ..Default::default()
        };

        let pool = mysql_async::Pool::new("mysql://username:password@toto:3306/db");

        let reward1 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info1)
            .await
            .unwrap();
        let reward2 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info2)
            .await
            .unwrap();
        let reward3 = get_user_reward(&pool, &boost_info, &proposal_info, &vote_info3)
            .await
            .unwrap();

        assert_eq!(
            reward1,
            U256::from(voting_power1 as u128) * pool_size / proposal_score
        );
        assert_eq!(
            reward2,
            U256::from(voting_power2 as u128) * pool_size / proposal_score
        );
        assert_eq!(
            reward3,
            U256::from(voting_power3 as u128) * pool_size / proposal_score
        );
    }
}
