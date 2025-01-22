use cosmwasm_schema::{cw_serde, schemars::JsonSchema};
use cosmwasm_std::{
    to_json_binary, Addr, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError, StdResult,
    SubMsg, Uint128, WasmMsg,
};

use cw_storage_plus::Item;
use semver::{Version, VersionReq};

use cw2::{get_contract_version, set_contract_version, ContractVersion};

use cw_denom::{CheckedDenom, UncheckedDenom};
use dao_interface::voting::{Query as CwCoreQuery, VotingPowerAtHeightResponse};
use dao_voting::{
    deposit::{CheckedDepositInfo, DepositRefundPolicy, UncheckedDepositInfo},
    pre_propose::{PreProposeSubmissionPolicy, PreProposeSubmissionPolicyError},
    status::Status,
};
use serde::Serialize;

use crate::{
    error::PreProposeError,
    helpers::add_and_remove_addresses,
    msg::{DepositInfoResponse, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg},
    state::{Config, PreProposeContract},
};

const CONTRACT_NAME: &str = "crates.io::dao-pre-propose-base";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

impl<InstantiateExt, ExecuteExt, QueryExt, MigrateExt, ProposalMessage>
    PreProposeContract<InstantiateExt, ExecuteExt, QueryExt, MigrateExt, ProposalMessage>
where
    ProposalMessage: Serialize,
    QueryExt: JsonSchema,
    MigrateExt: JsonSchema,
{
    pub fn instantiate(
        &self,
        deps: DepsMut,
        _env: Env,
        info: MessageInfo,
        msg: InstantiateMsg<InstantiateExt>,
    ) -> Result<Response, PreProposeError> {
        set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

        // The proposal module instantiates us. We're
        // making limited assumptions here. The only way to associate
        // a deposit module with a proposal module is for the proposal
        // module to instantiate it.
        self.proposal_module.save(deps.storage, &info.sender)?;

        // Query the proposal module for its DAO.
        let dao: Addr = deps
            .querier
            .query_wasm_smart(info.sender.clone(), &CwCoreQuery::Dao {})?;

        self.dao.save(deps.storage, &dao)?;

        let deposit_info = msg
            .deposit_info
            .map(|info| info.into_checked(deps.as_ref(), dao.clone()))
            .transpose()?;

        msg.submission_policy.validate()?;

        let config = Config {
            deposit_info,
            submission_policy: msg.submission_policy,
        };

        self.config.save(deps.storage, &config)?;

        Ok(Response::default()
            .add_attribute("method", "instantiate")
            .add_attribute("proposal_module", info.sender.into_string())
            .add_attribute("deposit_info", format!("{:?}", config.deposit_info))
            .add_attribute(
                "submission_policy",
                config.submission_policy.human_readable(),
            )
            .add_attribute("dao", dao))
    }

    pub fn execute(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        msg: ExecuteMsg<ProposalMessage, ExecuteExt>,
    ) -> Result<Response, PreProposeError> {
        match msg {
            ExecuteMsg::Propose { msg } => self.execute_propose(deps, env, info, msg),
            ExecuteMsg::UpdateConfig {
                deposit_info,
                submission_policy,
            } => self.execute_update_config(deps, info, deposit_info, submission_policy),
            ExecuteMsg::UpdateSubmissionPolicy {
                denylist_add,
                denylist_remove,
                set_dao_members,
                allowlist_add,
                allowlist_remove,
            } => self.execute_update_submission_policy(
                deps,
                info,
                denylist_add,
                denylist_remove,
                set_dao_members,
                allowlist_add,
                allowlist_remove,
            ),
            ExecuteMsg::Withdraw { denom } => {
                self.execute_withdraw(deps.as_ref(), env, info, denom)
            }
            ExecuteMsg::AddProposalSubmittedHook { address } => {
                self.execute_add_proposal_submitted_hook(deps, info, address)
            }
            ExecuteMsg::RemoveProposalSubmittedHook { address } => {
                self.execute_remove_proposal_submitted_hook(deps, info, address)
            }
            ExecuteMsg::ProposalCompletedHook {
                proposal_id,
                new_status,
            } => self.execute_proposal_completed_hook(deps.as_ref(), info, proposal_id, new_status),

            ExecuteMsg::Extension { .. } => Ok(Response::default()),
        }
    }

    pub fn execute_propose(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        msg: ProposalMessage,
    ) -> Result<Response, PreProposeError> {
        self.check_can_submit(deps.as_ref(), info.sender.clone())?;

        let config = self.config.load(deps.storage)?;

        let deposit_messages = if let Some(ref deposit_info) = config.deposit_info {
            deposit_info.check_native_deposit_paid(&info)?;
            deposit_info.get_take_deposit_messages(&info.sender, &env.contract.address)?
        } else {
            vec![]
        };

        let proposal_module = self.proposal_module.load(deps.storage)?;

        // Snapshot the deposit using the ID of the proposal that we
        // will create.
        let next_id = deps.querier.query_wasm_smart(
            &proposal_module,
            &dao_interface::proposal::Query::NextProposalId {},
        )?;
        self.deposits.save(
            deps.storage,
            next_id,
            &(config.deposit_info, info.sender.clone()),
        )?;

        let propose_messsage = WasmMsg::Execute {
            contract_addr: proposal_module.into_string(),
            msg: to_json_binary(&msg)?,
            funds: vec![],
        };

        let hooks_msgs = self
            .proposal_submitted_hooks
            .prepare_hooks(deps.storage, |a| {
                let execute = WasmMsg::Execute {
                    contract_addr: a.into_string(),
                    msg: to_json_binary(&msg)?,
                    funds: vec![],
                };
                Ok(SubMsg::new(execute))
            })?;

        Ok(Response::default()
            .add_attribute("method", "execute_propose")
            .add_attribute("sender", info.sender)
            // It's important that the propose message is
            // first. Otherwise, a hook receiver could create a
            // proposal before us and invalidate our `NextProposalId
            // {}` query.
            .add_message(propose_messsage)
            .add_submessages(hooks_msgs)
            .add_messages(deposit_messages))
    }

    pub fn execute_update_config(
        &self,
        deps: DepsMut,
        info: MessageInfo,
        deposit_info: Option<UncheckedDepositInfo>,
        submission_policy: Option<PreProposeSubmissionPolicy>,
    ) -> Result<Response, PreProposeError> {
        let dao = self.dao.load(deps.storage)?;
        if info.sender != dao {
            return Err(PreProposeError::NotDao {});
        }

        let deposit_info = deposit_info
            .map(|d| d.into_checked(deps.as_ref(), dao))
            .transpose()?;

        if let Some(submision_policy) = &submission_policy {
            submision_policy.validate()?
        }

        self.config
            .update(deps.storage, |prev| -> Result<Config, PreProposeError> {
                let new_submission_policy = if let Some(submission_policy) = submission_policy {
                    submission_policy
                } else {
                    prev.submission_policy
                };

                Ok(Config {
                    deposit_info,
                    submission_policy: new_submission_policy,
                })
            })?;

        Ok(Response::default()
            .add_attribute("method", "update_config")
            .add_attribute("sender", info.sender))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_update_submission_policy(
        &self,
        deps: DepsMut,
        info: MessageInfo,
        denylist_add: Option<Vec<String>>,
        denylist_remove: Option<Vec<String>>,
        set_dao_members: Option<bool>,
        allowlist_add: Option<Vec<String>>,
        allowlist_remove: Option<Vec<String>>,
    ) -> Result<Response, PreProposeError> {
        let dao = self.dao.load(deps.storage)?;
        if info.sender != dao {
            return Err(PreProposeError::NotDao {});
        }

        let mut config = self.config.load(deps.storage)?;

        match config.submission_policy {
            PreProposeSubmissionPolicy::Anyone { mut denylist } => {
                // Error if other values that apply to Specific were set.
                if set_dao_members.is_some()
                    || allowlist_add.is_some()
                    || allowlist_remove.is_some()
                {
                    return Err(PreProposeError::SubmissionPolicy(
                        PreProposeSubmissionPolicyError::AnyoneInvalidUpdateFields {},
                    ));
                }

                add_and_remove_addresses(
                    deps.as_ref(),
                    &mut denylist,
                    denylist_add,
                    denylist_remove,
                )?;

                config.submission_policy = PreProposeSubmissionPolicy::Anyone { denylist };
            }
            PreProposeSubmissionPolicy::Specific {
                dao_members,
                mut allowlist,
                mut denylist,
            } => {
                let dao_members = if let Some(new_dao_members) = set_dao_members {
                    new_dao_members
                } else {
                    dao_members
                };

                add_and_remove_addresses(
                    deps.as_ref(),
                    &mut allowlist,
                    allowlist_add,
                    allowlist_remove,
                )?;
                add_and_remove_addresses(
                    deps.as_ref(),
                    &mut denylist,
                    denylist_add,
                    denylist_remove,
                )?;

                config.submission_policy = PreProposeSubmissionPolicy::Specific {
                    dao_members,
                    allowlist,
                    denylist,
                };
            }
        }

        config.submission_policy.validate()?;
        self.config.save(deps.storage, &config)?;

        Ok(Response::default()
            .add_attribute("method", "update_submission_policy")
            .add_attribute("sender", info.sender))
    }

    pub fn execute_withdraw(
        &self,
        deps: Deps,
        env: Env,
        info: MessageInfo,
        denom: Option<UncheckedDenom>,
    ) -> Result<Response, PreProposeError> {
        let dao = self.dao.load(deps.storage)?;
        if info.sender != dao {
            Err(PreProposeError::NotDao {})
        } else {
            let denom = match denom {
                Some(denom) => Some(denom.into_checked(deps)?),
                None => {
                    let config = self.config.load(deps.storage)?;
                    config.deposit_info.map(|d| d.denom)
                }
            };
            match denom {
                None => Err(PreProposeError::NoWithdrawalDenom {}),
                Some(denom) => {
                    let balance = denom.query_balance(&deps.querier, &env.contract.address)?;
                    if balance.is_zero() {
                        Err(PreProposeError::NothingToWithdraw {})
                    } else {
                        let withdraw_message = denom.get_transfer_to_message(&dao, balance)?;
                        Ok(Response::default()
                            .add_message(withdraw_message)
                            .add_attribute("method", "withdraw")
                            .add_attribute("receiver", &dao)
                            .add_attribute("denom", denom.to_string()))
                    }
                }
            }
        }
    }

    pub fn execute_add_proposal_submitted_hook(
        &self,
        deps: DepsMut,
        info: MessageInfo,
        address: String,
    ) -> Result<Response, PreProposeError> {
        let dao = self.dao.load(deps.storage)?;
        if info.sender != dao {
            return Err(PreProposeError::NotDao {});
        }

        let addr = deps.api.addr_validate(&address)?;
        self.proposal_submitted_hooks.add_hook(deps.storage, addr)?;

        Ok(Response::default())
    }

    pub fn execute_remove_proposal_submitted_hook(
        &self,
        deps: DepsMut,
        info: MessageInfo,
        address: String,
    ) -> Result<Response, PreProposeError> {
        let dao = self.dao.load(deps.storage)?;
        if info.sender != dao {
            return Err(PreProposeError::NotDao {});
        }

        // Validate address
        let addr = deps.api.addr_validate(&address)?;

        // Remove the hook
        self.proposal_submitted_hooks
            .remove_hook(deps.storage, addr)?;

        Ok(Response::default())
    }

    pub fn execute_proposal_completed_hook(
        &self,
        deps: Deps,
        info: MessageInfo,
        id: u64,
        new_status: Status,
    ) -> Result<Response, PreProposeError> {
        let proposal_module = self.proposal_module.load(deps.storage)?;
        if info.sender != proposal_module {
            return Err(PreProposeError::NotModule {});
        }

        // If we receive a proposal completed hook from a proposal
        // module, and it is not in one of these states, something
        // bizare has happened. In that event, this message errors
        // which ought to cause the proposal module to remove this
        // module and open proposal submission to anyone.
        if new_status != Status::Closed
            && new_status != Status::Executed
            && new_status != Status::Vetoed
        {
            return Err(PreProposeError::NotCompleted { status: new_status });
        }

        match self.deposits.may_load(deps.storage, id)? {
            Some((deposit_info, proposer)) => {
                let messages = if let Some(ref deposit_info) = deposit_info {
                    // Determine if refund can be issued
                    let should_refund_to_proposer =
                        match (new_status, deposit_info.clone().refund_policy) {
                            // If policy is refund only passed props, refund for executed status
                            (Status::Executed, DepositRefundPolicy::OnlyPassed) => true,
                            // Don't refund other statuses for OnlyPassed policy
                            (_, DepositRefundPolicy::OnlyPassed) => false,
                            // Refund if the refund policy is always refund
                            (_, DepositRefundPolicy::Always) => true,
                            // Don't refund if the refund is never refund
                            (_, DepositRefundPolicy::Never) => false,
                        };

                    if should_refund_to_proposer {
                        deposit_info.get_return_deposit_message(&proposer)?
                    } else {
                        // If the proposer doesn't get the deposit, the DAO does.
                        let dao = self.dao.load(deps.storage)?;
                        deposit_info.get_return_deposit_message(&dao)?
                    }
                } else {
                    // No deposit info for this proposal. Nothing to do.
                    vec![]
                };

                Ok(Response::default()
                    .add_attribute("method", "execute_proposal_completed_hook")
                    .add_attribute("proposal", id.to_string())
                    .add_attribute("deposit_info", to_json_binary(&deposit_info)?.to_string())
                    .add_messages(messages))
            }

            // If we do not have a deposit for this proposal it was
            // likely created before we were added to the proposal
            // module. In that case, it's not our problem and we just
            // do nothing.
            None => Ok(Response::default()
                .add_attribute("method", "execute_proposal_completed_hook")
                .add_attribute("proposal", id.to_string())),
        }
    }

    pub fn check_can_submit(&self, deps: Deps, who: Addr) -> Result<(), PreProposeError> {
        let config = self.config.load(deps.storage)?;

        match config.submission_policy {
            PreProposeSubmissionPolicy::Anyone { denylist } => {
                if !denylist.contains(&who) {
                    return Ok(());
                }
            }
            PreProposeSubmissionPolicy::Specific {
                dao_members,
                allowlist,
                denylist,
            } => {
                // denylist overrides all other settings
                if !denylist.contains(&who) {
                    // if on the allowlist, return early
                    if allowlist.contains(&who) {
                        return Ok(());
                    }

                    // check DAO membership only if not on the allowlist
                    if dao_members {
                        let dao = self.dao.load(deps.storage)?;
                        let voting_power: VotingPowerAtHeightResponse =
                            deps.querier.query_wasm_smart(
                                dao.into_string(),
                                &CwCoreQuery::VotingPowerAtHeight {
                                    address: who.into_string(),
                                    height: None,
                                },
                            )?;
                        if !voting_power.power.is_zero() {
                            return Ok(());
                        }
                    }
                }
            }
        }

        // all other cases are not allowed
        Err(PreProposeError::SubmissionPolicy(
            PreProposeSubmissionPolicyError::Unauthorized {},
        ))
    }

    pub fn query(&self, deps: Deps, _env: Env, msg: QueryMsg<QueryExt>) -> StdResult<Binary> {
        match msg {
            QueryMsg::ProposalModule {} => {
                to_json_binary(&self.proposal_module.load(deps.storage)?)
            }
            QueryMsg::Dao {} => to_json_binary(&self.dao.load(deps.storage)?),
            QueryMsg::Info {} => to_json_binary(&dao_interface::proposal::InfoResponse {
                info: cw2::get_contract_version(deps.storage)?,
            }),
            QueryMsg::Config {} => to_json_binary(&self.config.load(deps.storage)?),
            QueryMsg::DepositInfo { proposal_id } => {
                let (deposit_info, proposer) = self.deposits.load(deps.storage, proposal_id)?;
                to_json_binary(&DepositInfoResponse {
                    deposit_info,
                    proposer,
                })
            }
            QueryMsg::CanPropose { address } => {
                let addr = deps.api.addr_validate(&address)?;
                match self.check_can_submit(deps, addr) {
                    Ok(_) => to_json_binary(&true),
                    Err(err) => match err {
                        PreProposeError::SubmissionPolicy(
                            PreProposeSubmissionPolicyError::Unauthorized {},
                        ) => to_json_binary(&false),
                        PreProposeError::Std(err) => Err(err),
                        _ => Err(StdError::generic_err(format!(
                            "unexpected error: {:?}",
                            err
                        ))),
                    },
                }
            }
            QueryMsg::ProposalSubmittedHooks {} => {
                to_json_binary(&self.proposal_submitted_hooks.query_hooks(deps)?)
            }
            QueryMsg::QueryExtension { .. } => Ok(Binary::default()),
        }
    }

    pub fn migrate(
        &self,
        deps: DepsMut,
        msg: MigrateMsg<MigrateExt>,
    ) -> Result<Response, PreProposeError> {
        match msg {
            MigrateMsg::FromUnderV250 { policy } => {
                #[cw_serde]
                struct ConfigV241 {
                    /// Information about the deposit required to create a
                    /// proposal. If `None`, no deposit is required.
                    pub deposit_info: Option<CheckedDepositInfoV241>,
                    /// If false, only members (addresses with voting power) may
                    /// create proposals in the DAO. Otherwise, any address may
                    /// create a proposal so long as they pay the deposit.
                    pub open_proposal_submission: bool,
                }

                /// Counterpart to the `DepositInfo` struct which has been
                /// processed. This type should never be constructed literally
                /// and should always by built by calling `into_checked` on a
                /// `DepositInfo` instance.
                #[cw_serde]
                struct CheckedDepositInfoV241 {
                    /// The address of the cw20 token to be used for proposal
                    /// deposits.
                    pub denom: CheckedDenomV241,
                    /// The number of tokens that must be deposited to create a
                    /// proposal. This is validated to be non-zero if this
                    /// struct is constructed by converted via the
                    /// `into_checked` method on `DepositInfo`.
                    pub amount: Uint128,
                    /// The policy used for refunding proposal deposits.
                    pub refund_policy: DepositRefundPolicyV241,
                }

                #[cw_serde]
                enum DepositRefundPolicyV241 {
                    /// Deposits should always be refunded.
                    Always,
                    /// Deposits should only be refunded for passed proposals.
                    OnlyPassed,
                    /// Deposits should never be refunded.
                    Never,
                }

                /// A denom that has been checked to point to a valid asset.
                /// This enum should never be constructed literally and should
                /// always be built by calling `into_checked` on an
                /// `UncheckedDenom` instance.
                #[cw_serde]
                enum CheckedDenomV241 {
                    /// A native (bank module) asset.
                    Native(String),
                    /// A cw20 asset.
                    Cw20(Addr),
                }

                // all contracts >= v2.4.1 and < v2.5.0 have the same config
                let required_str = ">=2.4.1, <2.5.0";

                // ensure acceptable version
                let requirement = VersionReq::parse(required_str).unwrap();
                let ContractVersion { version, .. } = get_contract_version(deps.storage)?;
                let sem_version = Version::parse(&version).unwrap();

                if !requirement.matches(&sem_version) {
                    return Err(PreProposeError::CannotMigrateVersion {
                        required: required_str.to_string(),
                        actual: version.clone(),
                    });
                }

                let old_config = Item::<ConfigV241>::new("config").load(deps.storage)?;

                // if provided a policy to update with, use it
                let submission_policy = if let Some(submission_policy) = policy {
                    submission_policy

                    // otherwise convert old `open_proposal_submission` flag
                    // into new policy enum
                } else if old_config.open_proposal_submission {
                    PreProposeSubmissionPolicy::Anyone { denylist: vec![] }
                } else {
                    PreProposeSubmissionPolicy::Specific {
                        dao_members: true,
                        allowlist: vec![],
                        denylist: vec![],
                    }
                };

                submission_policy.validate()?;

                let deposit_info: Option<CheckedDepositInfo> =
                    old_config.deposit_info.map(|old| CheckedDepositInfo {
                        denom: match old.denom {
                            CheckedDenomV241::Cw20(address) => CheckedDenom::Cw20(address),
                            CheckedDenomV241::Native(denom) => CheckedDenom::Native(denom),
                        },
                        amount: old.amount,
                        refund_policy: match old.refund_policy {
                            DepositRefundPolicyV241::Always => DepositRefundPolicy::Always,
                            DepositRefundPolicyV241::Never => DepositRefundPolicy::Never,
                            DepositRefundPolicyV241::OnlyPassed => DepositRefundPolicy::OnlyPassed,
                        },
                    });

                self.config.save(
                    deps.storage,
                    &Config {
                        deposit_info,
                        submission_policy,
                    },
                )?;

                set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

                Ok(Response::default()
                    .add_attribute("action", "migrate")
                    .add_attribute("from", version)
                    .add_attribute("to", CONTRACT_VERSION))
            }
            MigrateMsg::Extension { .. } => Err(PreProposeError::Std(StdError::generic_err(
                "not implemented",
            ))),
        }
    }
}
