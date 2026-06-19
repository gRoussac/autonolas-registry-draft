#![allow(unexpected_cfgs)]
use anchor_lang::{
    prelude::*,
    solana_program::{
        program::invoke_signed,
        system_instruction::{self, transfer},
    },
    AccountDeserialize, Discriminator,
};
use solana_sha256_hasher::hash;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount};
use constants::*;
use error::ErrorCode;
use events::*;
use mpl_token_metadata::instructions::CreateMetadataAccountV3Builder;
use mpl_token_metadata::types::DataV2;
use pda::*;
use service_state::ServiceState;
use state::*;

mod constants;
pub mod error;
pub mod events;
mod pda;
mod service_state;
mod state;

declare_id!("9Q2mQxDLH91HLaQUYyxV5n9WhA1jzgVThJwfJTNqEUNP");

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct AgentParams {
    pub slots: u32,
    pub bond: u64,
}

#[program]
pub mod registry {

    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        name: String,
        symbol: String,
        base_uri: String,
        manager: Pubkey,
        drainer: Pubkey,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;
        let registry_wallet = &mut ctx.accounts.registry_wallet;

        registry.name = name;
        registry.symbol = symbol;
        registry.base_uri = base_uri;
        registry.owner = ctx.accounts.user.key();
        registry.manager = manager;
        registry.drainer = drainer;
        registry.total_supply = 0;
        registry.version = "1.0.0".into();
        registry.wallet_key = registry_wallet.key();

        let (_, bump_registry_wallet) = registry_wallet_pda(&registry.key(), ctx.program_id);

        registry.wallet_bump = bump_registry_wallet;

        Ok(())
    }

    pub fn create(
        ctx: Context<CreateService>,
        config_hash: [u8; 32],
        service_owner: Pubkey,
        threshold: Option<u32>,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        if registry.locked {
            return Err(ErrorCode::ReentrancyGuard.into());
        }

        registry.locked = true;

        // Check for the manager privilege for a service management
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        // Check for the non-empty service owner address
        if service_owner == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Check for zero config hash
        if config_hash == [0u8; 32] {
            return Err(ErrorCode::ZeroConfigHash.into());
        }

        let service_id = registry.total_supply + 1;

        let service = &mut ctx.accounts.service;
        service.service_id = service_id;
        service.service_owner = service_owner;
        service.security_deposit = 0;
        service.config_hash = config_hash;
        service.max_num_agent_instances = 0;
        service.num_agent_instances = 0;
        service.state = ServiceState::PreRegistration;

        if threshold.is_some() {
            service.threshold = threshold.unwrap_or_default();
        }

        emit!(CreateServiceEvent {
            service_id,
            config_hash
        });

        assert_eq!(service_owner, ctx.accounts.token_account.owner);

        // Mint the service token to the service_owner
        let seeds: &[&[u8]] = &[b"mint_auth", &[ctx.bumps.mint_auth]];
        let signer_seeds: &[&[&[u8]]] = &[seeds];

        let minter = &ctx.accounts.minter;

        let cpi_accounts = MintTo {
            mint: minter.to_account_info(),
            to: ctx.accounts.token_account.to_account_info(),
            authority: ctx.accounts.mint_auth.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_context = CpiContext::new(cpi_program, cpi_accounts).with_signer(signer_seeds);
        token::mint_to(cpi_context, 1)?;

        // metadata PDA
        let metadata_seeds = &[
            b"metadata",
            mpl_token_metadata::ID.as_ref(),
            &minter.key().to_bytes(),
        ];

        let (metadata_pda, _bump) =
            Pubkey::find_program_address(metadata_seeds, &mpl_token_metadata::ID);

        let uri = format!("{}{}", registry.base_uri, ctx.accounts.service.key());

        let data = DataV2 {
            name: registry.name.clone(),
            symbol: registry.symbol.clone(),
            uri,
            seller_fee_basis_points: 0,
            creators: None,
            collection: None,
            uses: None,
        };

        let ix = CreateMetadataAccountV3Builder::new()
            .metadata(metadata_pda)
            .mint(ctx.accounts.minter.key())
            .mint_authority(ctx.accounts.mint_auth.key())
            .payer(ctx.accounts.user.key())
            .update_authority(ctx.accounts.mint_auth.key(), false)
            .data(data)
            .is_mutable(true)
            .instruction();

        //
        // ✅ Invoke the metadata instruction

        msg!("metadata_pda: {:?}", metadata_pda);
        msg!("Minter: {:?}", ctx.accounts.minter.key());
        msg!("Mint auth: {:?}", ctx.accounts.mint_auth.key());
        msg!("User: {:?}", ctx.accounts.user.key());
        msg!("System Program: {:?}", ctx.accounts.system_program.key());

        invoke_signed(
            &ix,
            &[
                ctx.accounts.metadata.to_account_info(),
                ctx.accounts.minter.to_account_info(),
                ctx.accounts.mint_auth.to_account_info(),
                ctx.accounts.user.to_account_info(),
                ctx.accounts.token_metadata_program.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                // ctx.accounts.rent.to_account_info(),
            ],
            signer_seeds,
        )?;

        registry.total_supply = service_id;
        registry.locked = false;

        Ok(())
    }

    pub fn update(
        ctx: Context<UpdateService>,
        config_hash: [u8; 32],
        service_owner: Pubkey,
        threshold: Option<u32>,
    ) -> Result<()> {
        let service = &mut ctx.accounts.service;
        let registry = &mut ctx.accounts.registry;

        // Only the manager can update the service
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        // Validate that the provided service owner is the actual owner of the service
        if service.service_owner != service_owner {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Check if the service state is PreRegistration, only then can the service be updated
        if service.state != ServiceState::PreRegistration {
            return Err(ErrorCode::WrongServiceState.into());
        }

        // Ensure the config hash is not empty
        if config_hash == [0u8; 32] {
            return Err(ErrorCode::ZeroConfigHash.into());
        }

        ServiceRegistry::validate_threshold(service, threshold)?;

        // Update the service configuration hash
        let last_config_hash = service.config_hash;
        if last_config_hash != config_hash {
            service.config_hash = config_hash;
        }

        emit!(UpdateServiceEvent {
            service_id: service.service_id,
            config_hash
        });

        Ok(())
    }

    pub fn register_agent_ids_to_service<'info>(
        ctx: Context<'_, '_, 'info, 'info, RegisterAgentIdsToService<'info>>,
        service_owner: Pubkey,
        agent_ids: Vec<u32>,
        agent_params: Vec<AgentParams>,
        threshold: Option<u32>,
    ) -> Result<()> {
        ServiceRegistry::initial_checks(&agent_ids, &agent_params)?;

        let registry = &mut ctx.accounts.registry;
        let service = &mut ctx.accounts.service;

        // Check for the manager privilege for a service management
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::UninitializedAccount.into());
        }

        if service_owner != service.service_owner {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        let program_id = ctx.program_id;
        let user_account_info = ctx.accounts.user.to_account_info();
        let system_program_account_info = ctx.accounts.system_program.to_account_info();
        let service_agent_ids_index = &mut ctx.accounts.service_agent_ids_index;
        let mut remaining_accounts = ctx.remaining_accounts.iter();

        // Temp new state to rebuild service metadata
        let mut new_max_num_agent_instances: u32 = 0;
        let mut new_security_deposit = 0;

        for i in 0..agent_ids.len() {
            let agent_id = agent_ids[i];
            let params = &agent_params[i];

            let agent_param_account_info = next_account_info(&mut remaining_accounts)?;

            let (agent_param_pda, agent_param_bump) =
                agent_param_pda(service.service_id, agent_id, ctx.program_id);

            require!(
                agent_param_pda == agent_param_account_info.key(),
                ErrorCode::InvalidPda
            );

            //  CREATE OR UPDATE AGENT_PARAM
            if agent_param_account_info.data_is_empty() {
                let agent_param_seeds: &[&[u8]] = &[
                    b"agent_param",
                    &service.service_id.to_le_bytes(),
                    &agent_id.to_le_bytes(),
                    &[agent_param_bump],
                ];

                invoke_signed(
                    &system_instruction::create_account(
                        &ctx.accounts.user.key(),
                        &agent_param_pda,
                        Rent::get()?.minimum_balance(AgentParamAccount::LEN),
                        AgentParamAccount::LEN as u64,
                        program_id,
                    ),
                    &[
                        user_account_info.clone(),
                        agent_param_account_info.clone(),
                        system_program_account_info.clone(),
                    ],
                    &[agent_param_seeds],
                )?;

                require_keys_eq!(
                    *agent_param_account_info.owner,
                    *program_id,
                    ErrorCode::InvalidAccountOwner
                );
            }

            let mut agent_param_data: Account<AgentParamAccount> =
                Account::try_from_unchecked(agent_param_account_info)?;

            if params.slots == 0 && !agent_param_account_info.data_is_empty() {
                //  DELETION MODE

                **user_account_info.try_borrow_mut_lamports()? +=
                    agent_param_account_info.lamports();
                **agent_param_account_info.try_borrow_mut_lamports()? = 0;
                agent_param_account_info.data.borrow_mut().fill(0);

                ServiceRegistry::delete_agent_param_index(
                    &mut service_agent_ids_index.agent_ids,
                    agent_id,
                );
                continue;
            }

            agent_param_data.agent_id = agent_id;
            agent_param_data.slots = params.slots;
            agent_param_data.bond = params.bond;

            require!(
                service_agent_ids_index.agent_ids.len() < MAX_AGENT_IDS_PER_SERVICE,
                ErrorCode::MaxAgentIdPerServiceReached
            );

            ServiceRegistry::upsert_agent_param_index(
                &mut service_agent_ids_index.agent_ids,
                &agent_param_data,
            );

            let mut data = agent_param_account_info.try_borrow_mut_data()?;

            // ! Write discriminator, this is important to retrieve the account later
            let discriminator =
                &hash("account:AgentParamAccount".as_bytes())
                    .to_bytes()[..8];
            data[..8].copy_from_slice(discriminator);

            agent_param_data.serialize(&mut &mut data[8..])?;
        }

        //  Recompute `new_max_num_agent_instances` and `new_security_deposit`
        // After adding/updating the agent params, loop over all agent_ids in the service
        for params in &service_agent_ids_index.agent_ids {
            // Recompute max number of agent instances (total slots)
            new_max_num_agent_instances = new_max_num_agent_instances.saturating_add(params.slots);
            // Recompute security deposit (max bond)
            new_security_deposit = new_security_deposit.max(params.bond);
        }

        //  FINAL SERVICE STATE UPDATE
        service.max_num_agent_instances = new_max_num_agent_instances;
        service.security_deposit = new_security_deposit;

        ServiceRegistry::validate_threshold(service, threshold)?;

        // Emit the event with updated service data
        emit!(RegisterAgentIdsEvent {
            service_id: service.service_id,
            agent_ids,
            max_num_agent_instances: new_max_num_agent_instances,
            security_deposit: new_security_deposit,
        });

        Ok(())
    }

    pub fn delete_agent_id_to_service<'info>(
        ctx: Context<'_, '_, 'info, 'info, RegisterAgentIdsToService<'info>>,
        service_owner: Pubkey,
        agent_id: u32,
        threshold: Option<u32>,
    ) -> Result<()> {
        let agent_ids = vec![agent_id];
        let agent_params = vec![AgentParams { slots: 0, bond: 0 }];

        register_agent_ids_to_service(ctx, service_owner, agent_ids, agent_params, threshold)
    }

    pub fn add_agent_id_to_service<'info>(
        ctx: Context<'_, '_, 'info, 'info, RegisterAgentIdsToService<'info>>,
        service_owner: Pubkey,
        agent_id: u32,
        slots: u32,
        bond: u64,
        threshold: Option<u32>,
    ) -> Result<()> {
        let agent_ids = vec![agent_id];
        let agent_params = vec![AgentParams { slots, bond }];

        register_agent_ids_to_service(ctx, service_owner, agent_ids, agent_params, threshold)
    }

    pub fn deploy<'info>(
        ctx: Context<'_, '_, 'info, 'info, Deploy<'info>>,
        service_id: u128,
        multisig_implementation: Pubkey,
        data: Vec<u8>,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        // Reentrancy guard (same concept as Solidity, using a lock mechanism)
        if registry.locked {
            return Err(ErrorCode::ReentrancyGuard.into());
        }
        registry.locked = true;

        let service = &mut ctx.accounts.service;
        let service_owner = &ctx.accounts.service_owner;

        let registry_multisig = &ctx.accounts.registry_multisig;

        // Check for the manager privilege for a service management
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        // Validate that the provided service owner is the actual owner of the service
        if service.service_owner != service_owner.key() {
            return Err(ProgramError::InvalidArgument.into());
        }

        require_eq!(service.service_id, service_id);

        // Check for whitelisted multisig implementation
        require!(
            registry_multisig.is_authorized(&multisig_implementation),
            ErrorCode::UnauthorizedMultisig
        );

        // Retrieve agent instances
        let agent_instances: Vec<Pubkey> = ctx
            .remaining_accounts
            .iter()
            .skip(1)
            .map(|acc| acc.key())
            .collect();

        let remaining_accounts = ctx.remaining_accounts;

        let multisig_pda = ServiceRegistry::create_multisig(
            &multisig_implementation, // call this implemenation later instead of create_multisig
            &agent_instances,
            service.threshold,
            &data,
            &ctx.accounts.user,
            ctx.program_id,
            remaining_accounts,
        )?;

        // Update service state
        service.multisig = multisig_pda;
        service.state = ServiceState::Deployed;

        emit!(DeployServiceEvent {
            service_id,
            multisig: multisig_pda,
        });

        registry.locked = false;

        Ok(())
    }

    pub fn change_drainer(ctx: Context<ChangeDrainer>, new_drainer: Pubkey) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        // Only owner can call
        if ctx.accounts.user.key() != registry.owner {
            return Err(ProgramError::IllegalOwner.into());
        }

        // Cannot set zero address
        if new_drainer == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        registry.drainer = new_drainer;

        emit!(DrainerUpdatedEvent { new_drainer });

        Ok(())
    }

    pub fn change_owner(ctx: Context<ChangeOwner>, new_owner: Pubkey) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        // Only current owner can call
        if ctx.accounts.user.key() != registry.owner {
            return Err(Error::from(ProgramError::IllegalOwner));
        }

        // Cannot set zero address
        if new_owner == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Update the owner
        registry.owner = new_owner;

        emit!(OwnerUpdatedEvent { new_owner });

        Ok(())
    }

    pub fn change_manager(ctx: Context<ChangeManager>, new_manager: Pubkey) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        // Only current owner can call
        if ctx.accounts.user.key() != registry.owner {
            return Err(Error::from(ProgramError::IllegalOwner));
        }

        // Cannot set zero address
        if new_manager == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Update the owner
        registry.manager = new_manager;

        emit!(ManagerUpdatedEvent { new_manager });

        Ok(())
    }

    pub fn set_base_uri(ctx: Context<ChangeManager>, new_base_uri: String) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        // Only current owner can call
        if ctx.accounts.user.key() != registry.owner {
            return Err(Error::from(ProgramError::IllegalOwner));
        }

        // Cannot set zero address
        if new_base_uri.is_empty() {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Update the owner
        registry.base_uri = new_base_uri.clone();

        emit!(BaseURIChanged { new_base_uri });

        Ok(())
    }

    // ! TODO This function is for bypassing mulitisg in tests, shall be remove
    // ! ONLY ALLOW THIS IN TEST ENV
    #[cfg(feature = "test-env")]
    pub fn change_multisig(ctx: Context<ChangeMultiSig>, new_multisig: Pubkey) -> Result<()> {
        let service = &mut ctx.accounts.service;

        // Only current service owner can call
        if ctx.accounts.user.key() != service.service_owner {
            return Err(Error::from(ProgramError::IllegalOwner));
        }

        // Cannot set zero address
        if new_multisig == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Update the service multisig !!!
        service.multisig = new_multisig;

        emit!(MultisigUpdatedEvent {
            service_id: service.service_id,
            new_multisig
        });
        Ok(())
    }

    pub fn drain(ctx: Context<Drain>) -> Result<u64> {
        let registry = &mut ctx.accounts.registry;
        let drainer = &ctx.accounts.drainer;

        // Reentrancy guard (same concept as Solidity, using a lock mechanism)
        if registry.locked {
            return Err(ErrorCode::ReentrancyGuard.into());
        }
        registry.locked = true;

        // Check if the caller is the correct drainer
        if drainer.key() != registry.drainer {
            return Err(ProgramError::IllegalOwner.into());
        }

        let amount = registry.slashed_funds;
        if amount > 0 {
            registry.slashed_funds = 0;

            let (registry_wallet_pda, registry_wallet_bump) =
                registry_wallet_pda(&registry.key(), ctx.program_id);

            require_eq!(
                registry_wallet_pda,
                ctx.accounts.registry_wallet.key(),
                ErrorCode::WrongRegistryWallet
            );

            require_eq!(
                registry_wallet_pda,
                registry.wallet_key,
                ErrorCode::WrongRegistryWallet
            );

            require_eq!(
                registry_wallet_bump,
                registry.wallet_bump,
                ErrorCode::WrongRegistryWallet
            );

            let registry_wallet_info = ctx.accounts.registry_wallet.to_account_info();
            let drainer_info = ctx.accounts.drainer.to_account_info();

            require!(
                registry_wallet_info.is_writable && drainer_info.is_writable,
                ErrorCode::AccountNotWritable
            );

            let mut from_lamports = registry_wallet_info.try_borrow_mut_lamports()?;
            let mut to_lamports = drainer_info.try_borrow_mut_lamports()?;

            if **from_lamports < amount {
                return Err(ErrorCode::InsufficientFunds.into());
            }

            **from_lamports -= &amount;
            **to_lamports += &amount;

            emit!(DrainEvent {
                drainer: ctx.accounts.drainer.key(),
                amount,
            });
        }

        registry.locked = false;

        Ok(amount)
    }

    pub fn slash<'info>(
        ctx: Context<'_, '_, 'info, 'info, Slash<'info>>,
        service_id: u128,
        agent_instances: Vec<Pubkey>,
        amounts: Vec<u64>,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;
        let service = &ctx.accounts.service;

        require_eq!(service.service_id, service_id);

        require!(
            service.state == ServiceState::Deployed,
            ErrorCode::WrongServiceState
        );
        require!(
            agent_instances.len() == amounts.len(),
            ErrorCode::WrongArrayLength
        );
        require_keys_eq!(
            ctx.accounts.user.key(),
            service.multisig,
            ErrorCode::OnlyOwnServiceMultisig
        );

        let mut remaining_accounts = ctx.remaining_accounts.iter();

        for (i, agent_instance) in agent_instances.iter().enumerate() {
            let amount_to_slash = amounts[i];

            // Get the OperatorAgentInstanceAccount
            let operator_agent_instance_info = next_account_info(&mut remaining_accounts)?;

            let operator_account: Account<OperatorAgentInstanceAccount> =
                Account::try_from(operator_agent_instance_info)?;
            let operator = operator_account.operator;

            let (operator_agent_instance_pda, _operator_agent_instance_bump) =
                operator_agent_instance_pda(agent_instance, &operator, ctx.program_id);

            require!(
                operator_agent_instance_pda == operator_agent_instance_info.key(),
                ErrorCode::InvalidPda
            );

            // Get the operator bond
            let operator_bond_info = next_account_info(&mut remaining_accounts)?;
            let mut operator_bond_account: Account<OperatorBondAccount> =
                Account::try_from(operator_bond_info)?;

            let (operator_bond_pda, _operator_bond_bump) =
                operator_bond_pda(service_id, &operator.key(), ctx.program_id);

            require!(
                operator_bond_pda == operator_bond_info.key(),
                ErrorCode::InvalidPda
            );

            require!(
                operator_bond_account.operator == operator,
                ErrorCode::WrongOperator
            );

            let current_bond = operator_bond_account.bond;

            // Slash logic

            require!(amount_to_slash > 0, ErrorCode::InvalidSlashAmount);
            require!(
                operator_bond_account.bond > 0,
                ErrorCode::IncorrectAgentBondingValue
            );

            let slashed_amount = std::cmp::min(current_bond, amount_to_slash);

            operator_bond_account.bond -= slashed_amount;
            let mut data = operator_bond_info.try_borrow_mut_data()?;
            let discriminator =
                &hash("account:OperatorBondAccount".as_bytes())
                    .to_bytes()[..8];
            data[..8].copy_from_slice(discriminator);
            operator_bond_account.serialize(&mut &mut data[8..])?;

            registry.slashed_funds += slashed_amount;

            emit!(OperatorSlashed {
                service_id,
                operator,
                amount: slashed_amount,
            });
        }

        Ok(())
    }

    pub fn check_service(ctx: Context<CheckService>, service_id: u128) -> Result<()> {
        let service_account = &ctx.accounts.service;
        // Find the Service Account PDA
        let (service_pda, _bump) = service_pda(&service_account.config_hash, ctx.program_id);

        // Use the service PDA to load the service account
        // Ensure the service account is the expected one
        require!(service_account.key() == service_pda, ErrorCode::InvalidPda);
        require_eq!(service_account.service_id, service_id);

        let service_agent_ids_index = &mut ctx.accounts.service_agent_ids_index;

        let (expected_pda, _bump) =
            service_agent_ids_index_pda(service_account.service_id, ctx.program_id);

        require!(
            service_agent_ids_index.key() == expected_pda,
            ErrorCode::InvalidPda
        );

        require!(
            !service_agent_ids_index.agent_ids.is_empty(),
            ErrorCode::InvalidServiceAgentPda
        );

        Ok(())
    }

    pub fn activate_registration(
        ctx: Context<ActivateRegistration>,
        service_id: u128,
        service_owner: Pubkey,
    ) -> Result<()> {
        let registry = &ctx.accounts.registry;
        let service = &mut ctx.accounts.service;

        // Check for the manager privilege for a service management
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        // Check for the non-empty service owner address
        if service_owner == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        require_eq!(service.service_id, service_id);

        require!(
            service.state == ServiceState::PreRegistration,
            ErrorCode::ServiceMustBeInactive
        );

        // Check that the deposit is the expected amount
        let user_pre_balance = ctx.accounts.user.lamports();
        require!(
            user_pre_balance >= service.security_deposit,
            ErrorCode::IncorrectRegistrationDepositValue
        );

        // Transfer the bond from user account to the program's wallet
        let (registry_wallet_pda, registry_wallet_bump) =
            registry_wallet_pda(&registry.key(), ctx.program_id);

        require_eq!(
            registry_wallet_pda,
            ctx.accounts.registry_wallet.key(),
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_pda,
            registry.wallet_key,
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_bump,
            registry.wallet_bump,
            ErrorCode::WrongRegistryWallet
        );

        let transfer_amount = service.security_deposit;

        let transfer_tx = transfer(
            &ctx.accounts.user.key(),
            &registry.wallet_key,
            transfer_amount,
        );

        // Execute the transfer
        invoke_signed(
            &transfer_tx,
            &[
                ctx.accounts.user.to_account_info(),
                ctx.accounts.registry_wallet.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[&[
                b"registry_wallet",
                registry.key().as_ref(),
                &[registry_wallet_bump],
            ]],
        )?;

        // Verify exact amount was transferred
        let user_post_balance = ctx.accounts.user.lamports();

        let balance_diff = user_pre_balance
            .checked_sub(user_post_balance)
            .ok_or(ErrorCode::Overflow)?;

        require!(
            balance_diff == transfer_amount,
            ErrorCode::IncorrectRegistrationDepositValue
        );

        service.state = ServiceState::ActiveRegistration;

        emit!(ActivateRegistrationEvent { service_id });

        Ok(())
    }

    pub fn register_agents<'info>(
        ctx: Context<'_, '_, 'info, 'info, RegisterAgentInstances<'info>>,
        operator: Pubkey,
        agent_instances: Vec<Pubkey>,
        agent_ids: Vec<u32>,
    ) -> Result<()> {
        let registry = &ctx.accounts.registry;

        // Permissions & State Checks
        ServiceRegistry::check_access_and_state(
            &ctx,
            registry,
            &ctx.accounts.service.state,
            &agent_instances,
            &agent_ids,
        )?;

        let service = &mut ctx.accounts.service;
        let mut remaining_accounts = ctx.remaining_accounts.iter();

        // Extract & Validate Agent Params
        let (agent_params, total_bond) =
            ServiceRegistry::load_and_validate_agent_params(&mut remaining_accounts, &agent_ids)?;

        // Transfer Bond
        ServiceRegistry::transfer_bond(
            ctx.program_id,
            &ctx.accounts.user,
            &ctx.accounts.system_program,
            registry,
            &ctx.accounts.registry_wallet,
            total_bond,
        )?;

        // Validate Operator
        ServiceRegistry::validate_operator(*ctx.program_id, operator, &mut remaining_accounts)?;

        let program_id = ctx.program_id;
        let user_account_info = ctx.accounts.user.to_account_info();
        let agent_instances_account_info = next_account_info(&mut remaining_accounts)?;
        let system_program_account_info = ctx.accounts.system_program.to_account_info();
        let operator_agent_instance_index = &mut ctx.accounts.operator_agent_instance_index;

        for (i, agent_id) in agent_ids.iter().enumerate() {
            let agent_instance = agent_instances[i];
            let agent_param = &agent_params[i];

            ServiceRegistry::register_single_instance(
                program_id,
                service,
                *agent_id,
                agent_instance,
                agent_param,
                operator,
                &user_account_info,
                agent_instances_account_info,
                &system_program_account_info,
                operator_agent_instance_index,
                &mut remaining_accounts,
            )?;
        }

        // Finalize service state if full
        if service.num_agent_instances == service.max_num_agent_instances {
            service.state = ServiceState::FinishedRegistration;
        }

        // Extract the operator_bond account from remaining_accounts
        let operator_bond_account_info = next_account_info(&mut remaining_accounts)?;

        // Update operator bond account
        ServiceRegistry::update_operator_bond(
            program_id,
            operator,
            service.service_id,
            total_bond,
            &ctx.accounts.user,
            operator_bond_account_info,
            &ctx.accounts.system_program,
        )?;

        Ok(())
    }

    pub fn terminate<'info>(
        ctx: Context<'_, '_, 'info, 'info, TerminateService<'info>>,
        service_id: u128,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;
        let service = &mut ctx.accounts.service;
        let service_owner = &ctx.accounts.service_owner;
        let service_agent_ids_index = &mut ctx.accounts.service_agent_ids_index;

        // Reentrancy guard
        if registry.locked {
            return Err(ErrorCode::ReentrancyGuard.into());
        }
        registry.locked = true;

        // Check for the manager privilege for service management
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        // Validate that the provided service owner is the actual owner of the service
        if service.service_owner != service_owner.key() {
            return Err(ProgramError::InvalidArgument.into());
        }

        require_eq!(service_id, service.service_id);

        // Check if already terminated
        require!(
            service.state != ServiceState::PreRegistration
                && service.state != ServiceState::TerminatedBonded,
            ErrorCode::WrongServiceState
        );

        // Update service state
        if service.num_agent_instances > 0 {
            service.state = ServiceState::TerminatedBonded;
        } else {
            service.state = ServiceState::PreRegistration;
        }

        let mut remaining_accounts_iter = ctx.remaining_accounts.iter();

        let (agent_instances_pda, _) =
            agent_instances_index_pda(service.service_id, ctx.program_id);

        let agent_instances_info = next_account_info(&mut remaining_accounts_iter)?;
        require!(
            agent_instances_info.key() == agent_instances_pda,
            ErrorCode::InvalidPda
        );

        // Cleanup all agent instance PDAs
        for param in &service_agent_ids_index.agent_ids {
            // Close the slot_counter PDA
            let (slot_counter_pda, _) =
                service_agent_slot_counter_pda(service.service_id, param.agent_id, ctx.program_id);

            let slot_counter_info = next_account_info(&mut remaining_accounts_iter)?;
            require!(
                slot_counter_info.key() == slot_counter_pda,
                ErrorCode::InvalidPda
            );
            ServiceRegistry::close_account(slot_counter_info, &ctx.accounts.user)?;

            // Now close all service_agent_instance PDAs for each agent_instance
            let agent_instances_account_index: Account<ServiceAgentInstancesIndex> =
                Account::try_from(agent_instances_info)?;

            for agent_instance in agent_instances_account_index.service_agent_instances.iter() {
                let (service_agent_instance_pda, _) = service_agent_instance_pda(
                    service.service_id,
                    param.agent_id,
                    agent_instance,
                    ctx.program_id,
                );

                let service_agent_instance_info = next_account_info(&mut remaining_accounts_iter)?;
                require!(
                    service_agent_instance_info.key() == service_agent_instance_pda,
                    ErrorCode::InvalidPda
                );
                ServiceRegistry::close_account(service_agent_instance_info, &ctx.accounts.user)?;
            }
        }

        // Close the agent_instances PDA
        ServiceRegistry::close_account(agent_instances_info, &ctx.accounts.user)?;

        // Refund security deposit
        let refund = service.security_deposit;

        let (registry_wallet_pda, registry_wallet_bump) =
            registry_wallet_pda(&registry.key(), ctx.program_id);

        require_eq!(
            registry_wallet_pda,
            ctx.accounts.registry_wallet.key(),
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_pda,
            registry.wallet_key,
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_bump,
            registry.wallet_bump,
            ErrorCode::WrongRegistryWallet
        );

        if refund > 0 {
            service.security_deposit = 0;

            let wallet_balance = ctx.accounts.registry_wallet.lamports();
            require!(wallet_balance >= refund, ErrorCode::InsufficientFunds);

            **ctx.accounts.registry_wallet.try_borrow_mut_lamports()? -= refund;
            **ctx.accounts.service_owner.try_borrow_mut_lamports()? += refund;

            emit!(Refunded {
                service_owner: ctx.accounts.service_owner.key(),
                amount: refund,
            });
        }

        service_agent_ids_index.agent_ids.clear();
        if service_agent_ids_index.agent_ids.is_empty() {
            ServiceRegistry::close_account(
                &service_agent_ids_index.to_account_info(),
                &ctx.accounts.user,
            )?;
        }

        emit!(ServiceTerminated { service_id });
        registry.locked = false;

        Ok(())
    }

    pub fn unbond<'info>(
        ctx: Context<'_, '_, 'info, 'info, UnbondOperator<'info>>,
        service_id: u128,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;
        let service = &mut ctx.accounts.service;
        let operator = &mut ctx.accounts.operator;
        let operator_bond = &mut ctx.accounts.operator_bond;

        if registry.locked {
            return Err(ErrorCode::ReentrancyGuard.into());
        }

        registry.locked = true;

        require_eq!(service_id, service.service_id);

        // Check for the manager privilege for a service management
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        // Check for the non-empty service owner address
        if operator.key() == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        // Validate service state
        require!(
            service.state == ServiceState::TerminatedBonded,
            ErrorCode::WrongServiceState
        );

        // Load agent instances for the operator
        let operator_agent_instance_index = &mut ctx.accounts.operator_agent_instance_index;

        let num_instances = operator_agent_instance_index.operator_agent_instances.len();
        require!(num_instances > 0, ErrorCode::OperatorHasNoInstances);

        // Update service state
        service.num_agent_instances = service
            .num_agent_instances
            .saturating_sub(num_instances as u32);
        if service.num_agent_instances == 0 {
            service.state = ServiceState::PreRegistration;
        }

        // Refund logic
        let (operator_bond_pda, _operator_bond_bump) =
            operator_bond_pda(service_id, &operator.key(), ctx.program_id);

        require!(
            operator_bond_pda == operator_bond.key(),
            ErrorCode::InvalidPda
        );

        let refund: u64 = operator_bond.bond;

        let (registry_wallet_pda, registry_wallet_bump) =
            registry_wallet_pda(&registry.key(), ctx.program_id);

        require_eq!(
            registry_wallet_pda,
            ctx.accounts.registry_wallet.key(),
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_pda,
            registry.wallet_key,
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_bump,
            registry.wallet_bump,
            ErrorCode::WrongRegistryWallet
        );

        // Only proceed if there's something to refund
        if refund > 0 {
            let wallet_balance = ctx.accounts.registry_wallet.lamports();
            require!(wallet_balance >= refund, ErrorCode::InsufficientFunds);

            operator_bond.bond = 0; // wipe the data

            // Transfer lamports back to the operator
            **operator.to_account_info().try_borrow_mut_lamports()? += refund;
            **ctx.accounts.registry_wallet.try_borrow_mut_lamports()? -= refund;

            // msg!("Refunded {} lamports to operator", refund);
        }

        ServiceRegistry::close_account(&operator_bond.to_account_info(), &ctx.accounts.user)?;

        for operator_agent_instance_pda in operator_agent_instance_index
            .operator_agent_instances
            .iter()
        {
            let operator_agent_instance_info =
                next_account_info(&mut ctx.remaining_accounts.iter())?;

            require!(
                operator_agent_instance_pda == &operator_agent_instance_info.key(),
                ErrorCode::InvalidPda
            );

            ServiceRegistry::close_account(operator_agent_instance_info, &ctx.accounts.user)?;
        }

        operator_agent_instance_index
            .operator_agent_instances
            .clear();

        if operator_agent_instance_index
            .operator_agent_instances
            .is_empty()
        {
            ServiceRegistry::close_account(
                &operator_agent_instance_index.to_account_info(),
                &ctx.accounts.user,
            )?;
        }

        // Emit event
        emit!(OperatorUnbonded {
            operator: operator.key(),
            service_id,
            refund,
        });

        registry.locked = false;
        Ok(())
    }

    pub fn change_multisig_permission(
        ctx: Context<ChangeMultisigPermission>,
        multisig: Pubkey,
        permission: bool,
    ) -> Result<()> {
        let registry = &mut ctx.accounts.registry;

        if ctx.accounts.user.key() != registry.owner {
            return Err(ProgramError::IllegalOwner.into());
        }

        if multisig == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        let registry_multisig = &mut ctx.accounts.registry_multisig;

        if permission && registry_multisig.authorized_multisigs.len() >= MAX_MULTISIGS {
            return Err(ErrorCode::MaxMultiSigsReached.into());
        }

        if permission {
            if !registry_multisig.authorized_multisigs.contains(&multisig) {
                registry_multisig.authorized_multisigs.push(multisig);
            }
        } else {
            registry_multisig
                .authorized_multisigs
                .retain(|x| x != &multisig);
        }

        Ok(())
    }

    pub fn dummy_include_agent_param_account(
        _ctx: Context<DummyContextForAgentParam>,
    ) -> Result<()> {
        Ok(())
    }

    pub fn dummy_include_agent_instances(_ctx: Context<DummyReadAgentInstances>) -> Result<()> {
        Ok(())
    }

    pub fn dummy_include_operator_agent_instance_account(
        _ctx: Context<DummyOperatorAgentInstanceAccount>,
    ) -> Result<()> {
        Ok(())
    }

    pub fn dummy_include_service_agent_instance_account(
        _ctx: Context<DummyServiceAgentInstanceAccount>,
    ) -> Result<()> {
        Ok(())
    }

    pub fn dummy_include_service_agent_slot_counter_account(
        _ctx: Context<DummyServiceAgentSlotCounterAccount>,
    ) -> Result<()> {
        Ok(())
    }

    pub fn dummy_include_operator_bond_account(
        _ctx: Context<DummyOperatorBondAccount>,
    ) -> Result<()> {
        Ok(())
    }
}

impl ServiceRegistry {
    fn initial_checks(agent_ids: &[u32], agent_params: &[AgentParams]) -> Result<()> {
        // Check arrays are non-empty and of equal length
        if agent_ids.is_empty() || agent_ids.len() != agent_params.len() {
            return Err(ErrorCode::WrongArrayLength.into());
        }

        // Check agent IDs are strictly increasing (sorted without duplicates)
        let mut last_id: u32 = 0;
        for (i, &id) in agent_ids.iter().enumerate() {
            if i > 0 && id <= last_id {
                return Err(ErrorCode::WrongAgentId.into());
            }
            last_id = id;
        }

        // Check for zero values in slots or bond
        for (_, params) in agent_ids.iter().zip(agent_params.iter()) {
            if (params.slots == 0 && params.bond != 0) || (params.slots != 0 && params.bond == 0) {
                return Err(ErrorCode::ZeroValue.into());
            }
        }

        Ok(())
    }

    fn upsert_agent_param_index(vec: &mut Vec<AgentParamAccount>, param: &AgentParamAccount) {
        let param_clone = (*param).clone();
        if let Some(existing) = vec.iter_mut().find(|x| x.agent_id == param.agent_id) {
            *existing = param_clone;
        } else {
            vec.push(param_clone);
        }
    }

    fn delete_agent_param_index(vec: &mut Vec<AgentParamAccount>, agent_id: u32) {
        if let Some(index) = vec.iter().position(|x| x.agent_id == agent_id) {
            vec.remove(index);
        }
    }

    fn validate_threshold(service: &mut ServiceAccount, threshold: Option<u32>) -> Result<()> {
        // Set the threshold if provided
        if threshold.is_some() {
            service.threshold = threshold.unwrap_or_default();
        }

        // Calculate the check_threshold value based on max_num_agent_instances
        let check_threshold = (service.max_num_agent_instances * 2 + 1).div_ceil(3);

        // Validate the threshold value as per the condition provided
        if service.threshold < check_threshold {
            return Err(ErrorCode::WrongThreshold.into());
        }

        if service.threshold > service.max_num_agent_instances {
            return Err(ErrorCode::WrongThreshold2.into());
        }

        Ok(())
    }

    fn check_access_and_state<'info>(
        ctx: &Context<'_, '_, 'info, 'info, RegisterAgentInstances<'info>>,
        registry: &ServiceRegistry,
        service_state: &ServiceState,
        agent_instances: &[Pubkey],
        agent_ids: &[u32],
    ) -> Result<()> {
        if ctx.accounts.user.key() != registry.manager {
            return Err(ProgramError::InvalidAccountOwner.into());
        }

        require!(
            agent_instances.len() == agent_ids.len(),
            ErrorCode::WrongArrayLength
        );

        require!(
            service_state == &ServiceState::ActiveRegistration,
            ErrorCode::WrongServiceState
        );

        Ok(())
    }

    fn load_and_validate_agent_params<'info>(
        remaining_accounts: &mut std::slice::Iter<AccountInfo<'info>>,
        agent_ids: &Vec<u32>,
    ) -> Result<(Vec<AgentParamAccount>, u64)> {
        let mut agent_params = vec![];
        let mut total_bond = 0;

        for _ in agent_ids {
            let agent_param_account_info = next_account_info(remaining_accounts)?;
            let data = agent_param_account_info.try_borrow_data()?;
            let agent_param = AgentParamAccount::try_from_slice(&data[8..])?;

            require!(agent_param.slots > 0, ErrorCode::AgentNotInService);

            total_bond += agent_param.bond;
            agent_params.push(agent_param);
        }

        Ok((agent_params, total_bond))
    }

    fn transfer_bond<'info>(
        program_id: &Pubkey,
        user: &Signer<'info>,
        system_program: &Program<'info, System>,
        registry: &Account<'_, ServiceRegistry>,
        registry_wallet: &AccountInfo<'info>,
        transfer_amount: u64,
    ) -> Result<()> {
        let user_pre_balance = user.lamports();

        require!(
            user_pre_balance >= transfer_amount,
            ErrorCode::IncorrectRegistrationDepositValue
        );

        let (registry_wallet_pda, registry_wallet_bump) =
            registry_wallet_pda(&registry.key(), program_id);

        require_eq!(
            registry_wallet_pda,
            registry_wallet.key(),
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_pda,
            registry.wallet_key,
            ErrorCode::WrongRegistryWallet
        );

        require_eq!(
            registry_wallet_bump,
            registry.wallet_bump,
            ErrorCode::WrongRegistryWallet
        );

        let transfer_tx = transfer(&user.key(), &registry_wallet.key(), transfer_amount);

        invoke_signed(
            &transfer_tx,
            &[
                user.to_account_info(),
                registry_wallet.clone(),
                system_program.to_account_info(),
            ],
            &[&[
                b"registry_wallet",
                registry.key().as_ref(),
                &[registry_wallet_bump],
            ]],
        )?;

        let user_post_balance = user.lamports();

        let balance_diff = user_pre_balance
            .checked_sub(user_post_balance)
            .ok_or(ErrorCode::Overflow)?;

        require!(
            balance_diff == transfer_amount,
            ErrorCode::IncorrectRegistrationDepositValue
        );

        Ok(())
    }

    fn validate_operator<'info>(
        program_id: Pubkey,
        operator: Pubkey,
        remaining_accounts: &mut std::slice::Iter<AccountInfo<'info>>,
    ) -> Result<()> {
        if operator == Pubkey::default() {
            return Err(ProgramError::InvalidArgument.into());
        }

        let (operator_as_agent_pda, _) = operator_as_agent_pda(&operator, &program_id);

        let operator_check_account_info = next_account_info(remaining_accounts)?;

        require!(
            operator_as_agent_pda == operator_check_account_info.key(),
            ErrorCode::InvalidPda
        );

        require!(
            operator_check_account_info.data_is_empty(),
            ErrorCode::WrongOperator
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn register_single_instance<'info>(
        program_id: &Pubkey,
        service: &mut Account<ServiceAccount>,
        agent_id: u32,
        agent_instance: Pubkey,
        agent_param: &AgentParamAccount,
        operator: Pubkey,
        user_account_info: &AccountInfo<'info>,
        agent_instances_account_info_index: &'info AccountInfo<'info>,
        system_program_account_info: &AccountInfo<'info>,
        operator_agent_instance_index: &mut Account<'info, OperatorAgentInstanceIndex>,
        remaining_accounts: &mut std::slice::Iter<'info, AccountInfo<'info>>,
    ) -> Result<()> {
        let service_id = service.service_id;

        // 1. Global agent_instances
        let (agent_instances_pda, agent_instances_bump) =
            agent_instances_index_pda(service.service_id, program_id);
        require!(
            agent_instances_pda == agent_instances_account_info_index.key(),
            ErrorCode::InvalidPda
        );

        // Check if the account exists
        let mut agent_instances_index: Account<ServiceAgentInstancesIndex>;

        if agent_instances_account_info_index.data_is_empty() {
            // Create the account if it doesn't exist
            invoke_signed(
                &system_instruction::create_account(
                    &user_account_info.key(),
                    &agent_instances_pda,
                    Rent::get()?.minimum_balance(ServiceAgentInstancesIndex::LEN),
                    ServiceAgentInstancesIndex::LEN as u64,
                    program_id,
                ),
                &[
                    user_account_info.clone(),
                    agent_instances_account_info_index.clone(),
                    system_program_account_info.clone(),
                ],
                &[&[
                    b"agent_instances_index",
                    &service_id.to_le_bytes(),
                    &[agent_instances_bump],
                ]],
            )?;

            // Initialize new empty account
            agent_instances_index =
                Account::try_from_unchecked(agent_instances_account_info_index)?;
            agent_instances_index.service_agent_instances = Vec::new();
        } else {
            // Load existing account
            agent_instances_index =
                Account::try_from_unchecked(agent_instances_account_info_index)?;
        }

        // Add the new agent instance
        agent_instances_index
            .service_agent_instances
            .push(agent_instance);

        let mut data = agent_instances_account_info_index.try_borrow_mut_data()?;
        let discriminator = &hash(
            "account:ServiceAgentInstancesIndex".as_bytes(),
        )
        .to_bytes()[..8];
        data[..8].copy_from_slice(discriminator);

        // Serialize the struct after discriminator
        agent_instances_index.serialize(&mut &mut data[8..])?;

        //  2. Slot counter
        let (slot_counter_pda, slot_counter_bump) =
            service_agent_slot_counter_pda(service.service_id, agent_id, program_id);

        let slot_counter_info = next_account_info(remaining_accounts)?;
        require!(
            slot_counter_pda == slot_counter_info.key(),
            ErrorCode::InvalidPda
        );

        let mut slot_counter: Account<ServiceAgentSlotCounterAccount> =
            if slot_counter_info.data_is_empty() {
                invoke_signed(
                    &system_instruction::create_account(
                        &user_account_info.key(),
                        &slot_counter_pda,
                        Rent::get()?.minimum_balance(1 + 8),
                        1 + 8,
                        program_id,
                    ),
                    &[
                        user_account_info.clone(),
                        slot_counter_info.clone(),
                        system_program_account_info.clone(),
                    ],
                    &[&[
                        b"service_agent_slot",
                        &service_id.to_le_bytes(),
                        &agent_id.to_le_bytes(),
                        &[slot_counter_bump],
                    ]],
                )?;

                Account::<ServiceAgentSlotCounterAccount>::try_from_unchecked(slot_counter_info)?
            } else {
                Account::<ServiceAgentSlotCounterAccount>::try_from_unchecked(slot_counter_info)?
            };

        require!(
            slot_counter.count < agent_param.slots as u8,
            ErrorCode::AgentInstancesSlotsFilled
        );

        slot_counter.count += 1;
        let mut data = slot_counter_info.try_borrow_mut_data()?;
        let discriminator = &hash(
            "account:ServiceAgentSlotCounterAccount".as_bytes(),
        )
        .to_bytes()[..8];
        data[..8].copy_from_slice(discriminator);
        slot_counter.serialize(&mut &mut data[8..])?;

        //  3. service_agent_instance
        let (service_agent_instance_pda, service_agent_instance_bump) =
            service_agent_instance_pda(service.service_id, agent_id, &agent_instance, program_id);

        let service_agent_instance_account_info = next_account_info(remaining_accounts)?;
        require!(
            service_agent_instance_pda == service_agent_instance_account_info.key(),
            ErrorCode::InvalidPda
        );

        if !service_agent_instance_account_info.data_is_empty() {
            return Err(ErrorCode::AccountServiceAgentIdInstanceExists.into());
        }

        invoke_signed(
            &system_instruction::create_account(
                &user_account_info.key(),
                &service_agent_instance_pda,
                Rent::get()?.minimum_balance(ServiceAgentInstanceAccount::LEN),
                ServiceAgentInstanceAccount::LEN as u64,
                program_id,
            ),
            &[
                user_account_info.clone(),
                service_agent_instance_account_info.clone(),
                system_program_account_info.clone(),
            ],
            &[&[
                b"service_agent_instance_account",
                &service_id.to_le_bytes(),
                &agent_id.to_le_bytes(),
                &agent_instance.to_bytes(),
                &[service_agent_instance_bump],
            ]],
        )?;

        let mut service_agent_instance_data: Account<ServiceAgentInstanceAccount> =
            Account::try_from_unchecked(service_agent_instance_account_info)?;

        service_agent_instance_data.service_id = service_id;
        service_agent_instance_data.agent_id = agent_id;
        service_agent_instance_data.agent_instance = agent_instance;

        let mut data = service_agent_instance_account_info.try_borrow_mut_data()?;
        let discriminator = &hash(
            "account:ServiceAgentInstanceAccount".as_bytes(),
        )
        .to_bytes()[..8];
        data[..8].copy_from_slice(discriminator);
        service_agent_instance_data.serialize(&mut &mut data[8..])?;

        //  4. operator_agent_instance
        let (operator_agent_instance_pda, operator_agent_instance_bump) =
            operator_agent_instance_pda(&agent_instance, &operator, program_id);

        let operator_agent_instance_account_info = next_account_info(remaining_accounts)?;
        require!(
            operator_agent_instance_pda == operator_agent_instance_account_info.key(),
            ErrorCode::InvalidPda
        );

        if !operator_agent_instance_account_info.data_is_empty() {
            return Err(ErrorCode::AccountAgentIdInstanceOperatorExists.into());
        }

        invoke_signed(
            &system_instruction::create_account(
                &user_account_info.key(),
                &operator_agent_instance_pda,
                Rent::get()?.minimum_balance(OperatorAgentInstanceAccount::LEN),
                OperatorAgentInstanceAccount::LEN as u64,
                program_id,
            ),
            &[
                user_account_info.clone(),
                operator_agent_instance_account_info.clone(),
                system_program_account_info.clone(),
            ],
            &[&[
                b"operator_agent_instance",
                &agent_instance.to_bytes(),
                &operator.to_bytes(),
                &[operator_agent_instance_bump],
            ]],
        )?;

        let mut operator_agent_instance_data: Account<OperatorAgentInstanceAccount> =
            Account::try_from_unchecked(operator_agent_instance_account_info)?;
        operator_agent_instance_data.operator = operator;
        operator_agent_instance_data.service_agent_instance = service_agent_instance_pda;

        let mut data = operator_agent_instance_account_info.try_borrow_mut_data()?;
        let discriminator = &hash(
            "account:OperatorAgentInstanceAccount".as_bytes(),
        )
        .to_bytes()[..8];
        data[..8].copy_from_slice(discriminator);
        operator_agent_instance_data.serialize(&mut &mut data[8..])?;

        service.num_agent_instances += 1;
        require!(
            service.num_agent_instances <= service.max_num_agent_instances,
            ErrorCode::IncorrectAgentInstances
        );

        //  Push in operator_agent_instance_index
        let (operator_agent_instance_index_pda, _operator_agent_instance_index_bump) =
            operator_agent_instance_index_pda(service.service_id, &operator, program_id);

        require!(
            operator_agent_instance_index_pda == operator_agent_instance_index.key(),
            ErrorCode::InvalidPda
        );

        require!(
            operator_agent_instance_index.operator_agent_instances.len()
                < MAX_AGENT_INSTANCES_PER_SERVICE,
            ErrorCode::MaxAgentInstancesPerServiceReached
        );

        operator_agent_instance_index
            .operator_agent_instances
            .push(operator_agent_instance_pda);

        emit!(RegisterInstance {
            operator,
            service_id,
            agent_instance,
            agent_id,
        });

        Ok(())
    }

    fn update_operator_bond<'info>(
        program_id: &Pubkey,
        operator: Pubkey,
        service_id: u128,
        total_bond: u64,
        user_account_info: &AccountInfo<'info>,
        operator_bond_account_info: &'info AccountInfo<'info>,
        system_program_account_info: &AccountInfo<'info>,
    ) -> Result<()> {
        let (operator_bond_pda, operator_bond_bump) =
            operator_bond_pda(service_id, &operator.key(), program_id);

        require!(
            operator_bond_pda == operator_bond_account_info.key(),
            ErrorCode::InvalidPda
        );

        if operator_bond_account_info.data_is_empty() {
            invoke_signed(
                &system_instruction::create_account(
                    &user_account_info.key(),
                    &operator_bond_pda,
                    Rent::get()?.minimum_balance(OperatorBondAccount::LEN),
                    OperatorBondAccount::LEN as u64,
                    program_id,
                ),
                &[
                    user_account_info.clone(),
                    operator_bond_account_info.clone(),
                    system_program_account_info.clone(),
                ],
                &[&[
                    b"operator_bond",
                    &service_id.to_le_bytes(),
                    &operator.to_bytes(),
                    &[operator_bond_bump],
                ]],
            )?;
        }

        let mut operator_bond_data: Account<OperatorBondAccount> =
            Account::try_from_unchecked(operator_bond_account_info)?;

        operator_bond_data.service_id = service_id;
        operator_bond_data.operator = operator;
        operator_bond_data.bond += total_bond;

        let mut data = operator_bond_account_info.try_borrow_mut_data()?;
        let discriminator =
            &hash("account:OperatorBondAccount".as_bytes())
                .to_bytes()[..8];
        data[..8].copy_from_slice(discriminator);
        operator_bond_data.serialize(&mut &mut data[8..])?;

        emit!(Deposit {
            operator,
            amount: total_bond,
        });

        Ok(())
    }

    fn close_account<'info>(
        account: &AccountInfo<'info>,
        refund_to: &AccountInfo<'info>,
    ) -> Result<()> {
        let lamports = account.lamports();
        if lamports > 0 {
            **refund_to.try_borrow_mut_lamports()? += lamports;
            **account.try_borrow_mut_lamports()? = 0;
        }
        account.data.borrow_mut().fill(0);
        Ok(())
    }

    pub fn create_multisig<'info>(
        _multisig_implementation: &Pubkey,
        agent_instances: &[Pubkey],
        threshold: u32,
        data: &[u8],
        payer: &Signer<'info>,
        program_id: &Pubkey,
        remaining_accounts: &[AccountInfo<'info>],
    ) -> Result<Pubkey> {
        // TODO! here we normally call multisig_implementation but we mimic a IMultisig(multisigImplementation).create
        require!(
            threshold > 0 && threshold as usize <= agent_instances.len(),
            ErrorCode::WrongThreshold
        );

        // Prepare seed data based on agent instances
        let mut seed_data = vec![];
        for agent in agent_instances {
            seed_data.extend_from_slice(agent.as_ref());
        }

        let hash = hash(&seed_data);
        msg!(&hex::encode(hash));
        let seeds: &[&[u8]] = &[b"multisig", hash.as_ref()];

        //  assert_eq!(1, 2);

        // Generate the PDA for the multisig account
        let (multisig_pda, bump) = Pubkey::find_program_address(seeds, program_id); // multisig_implementation

        // Ensure that the multisig_pda is included in the accounts list
        let multisig_account_info = &remaining_accounts[0];

        msg!("multisig_pda {:?}", multisig_account_info.key());
        msg!("multisig_pda {:?}", multisig_pda);

        require_eq!(multisig_account_info.key(), multisig_pda);

        // Determine space and lamports for account creation
        let space = MultisigAccount::size(agent_instances.len(), data.len());
        let lamports = Rent::get()?.minimum_balance(space);

        // Create the instruction for account creation
        let ix = system_instruction::create_account(
            &payer.key(),
            &multisig_pda,
            lamports,
            space as u64,
            program_id, // multisig_implementation
        );

        // Use invoke_signed with the collected account info
        let signer_seeds: &[&[u8]] = &[b"multisig", hash.as_ref(), &[bump]];

        anchor_lang::solana_program::program::invoke_signed(
            &ix,
            &[payer.to_account_info(), multisig_account_info.clone()],
            &[signer_seeds],
        )?;

        let multisig_account_data = MultisigAccount {
            agent_instances: agent_instances.to_vec(),
            threshold,
            data: data.to_vec(),
        };

        multisig_account_info.try_borrow_mut_data()?;
        multisig_account_data.serialize(&mut *multisig_account_info.data.borrow_mut())?;

        // Return the multisig PDA (which is the address of the newly created account)
        Ok(multisig_pda)
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = user, space = REGISTRY_ACCOUNT_SIZE)]
    pub registry: Account<'info, ServiceRegistry>,

    /// CHECK: PDA wallet owned by the program
    #[account(
            init,
            payer = user,
            space = 8,
            seeds = [b"registry_wallet", registry.key().as_ref()],
            bump
        )]
    pub registry_wallet: AccountInfo<'info>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(config_hash: [u8; 32], service_owner: Pubkey)]
pub struct CreateService<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(
        init,
        payer = user,
        space = 8 + std::mem::size_of::<ServiceAccount>(),
        seeds = [b"service", &config_hash[..7]],
        bump,
    )]
    pub service: Account<'info, ServiceAccount>,

    #[account(mut, address = registry.manager)]
    pub user: Signer<'info>,

    /// CHECK: owner
    #[account(mut, address = service_owner)]
    pub mint_owner: AccountInfo<'info>,

    // /// CHECK: metadata
    // #[account(mut)]
    // pub metadata: UncheckedAccount<'info>,

    // /// CHECK: manually validated as Metaplex Token Metadata program
    // pub token_metadata_program: UncheckedAccount<'info>,
    /// CHECK: PDA mint authority
    #[account(seeds = [b"mint_auth"], bump)]
    pub mint_auth: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = user,
        seeds = [b"mint"],
        bump,
        mint::decimals = 0,
        mint::authority = mint_auth
    )]
    pub minter: Account<'info, Mint>,

    #[account(
        init_if_needed,
        payer = user,
        seeds = [b"token_account"],
        bump,
        token::mint = minter,
        token::authority = mint_owner
    )]
    pub token_account: Account<'info, TokenAccount>,

    /// CHECK: verified by Metaplex program
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,

    /// CHECK: verified by Metaplex program
    #[account(mut)]
    pub master_edition: UncheckedAccount<'info>,

    /// CHECK: verified by Metaplex program
    #[account(address = mpl_token_metadata::ID)]
    pub token_metadata_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct UpdateService<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    #[account(address = registry.manager)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterAgentIdsToService<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + (MAX_AGENT_IDS_PER_SERVICE * AgentParamAccount::LEN) + 8, // 8 bytes for Vec metadata + data for MAX_AGENT_IDS_PER_SERVICE u32 agent IDs + 8 bytes Vec overhead
        seeds = [b"service_agent_ids_index", &service.service_id.to_le_bytes()[..]],
        bump,
    )]
    pub service_agent_ids_index: Account<'info, ServiceAgentIdsIndex>,

    #[account(mut, address = registry.manager)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ChangeDrainer<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,
    #[account(address = registry.owner)]
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct ChangeOwner<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct ChangeManager<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct ChangeMultiSig<'info> {
    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct CheckService<'info> {
    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    pub service_agent_ids_index: Account<'info, ServiceAgentIdsIndex>,
}

#[derive(Accounts)]
pub struct ActivateRegistration<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    /// CHECK: PDA wallet owned by the program
    #[account(mut, address = registry.wallet_key)]
    pub registry_wallet: AccountInfo<'info>,

    #[account(mut, address = registry.manager)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(operator: Pubkey)]
pub struct RegisterAgentInstances<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    /// CHECK: PDA wallet owned by the program
    #[account(mut, address = registry.wallet_key)]
    pub registry_wallet: AccountInfo<'info>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + (MAX_AGENT_INSTANCES_PER_SERVICE * PUBKEY_SIZE) + 8, // 8 bytes for Vec metadata + data for MAX_AGENT_INSTANCES_PER_SERVICE PUBKEY_SIZE operator_agent_instance_pda PDA + 8 bytes Vec overhead
        seeds = [b"operator_agent_instance_index", &service.service_id.to_le_bytes()[..], &operator.to_bytes()[..]],
        bump,
    )]
    pub operator_agent_instance_index: Account<'info, OperatorAgentInstanceIndex>,

    #[account(mut, address = registry.manager)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct TerminateService<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    #[account(mut)]
    pub service_agent_ids_index: Account<'info, ServiceAgentIdsIndex>,

    /// CHECK: PDA wallet owned by the program
    #[account(mut, address = registry.wallet_key)]
    pub registry_wallet: AccountInfo<'info>,

    /// CHECK: service_owner
    #[account(mut, address = service.service_owner)]
    pub service_owner: AccountInfo<'info>,

    #[account(mut, address = registry.manager)]
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct UnbondOperator<'info> {
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    #[account(mut)]
    pub operator_agent_instance_index: Account<'info, OperatorAgentInstanceIndex>,

    #[account(mut)]
    pub operator_bond: Account<'info, OperatorBondAccount>,

    /// CHECK: operator
    #[account(mut)]
    pub operator: AccountInfo<'info>,

    /// CHECK: PDA wallet owned by the program
    #[account(mut, address = registry.wallet_key)]
    pub registry_wallet: AccountInfo<'info>,

    #[account(mut, address = registry.manager)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Drain<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub drainer: Signer<'info>,

    /// CHECK: PDA wallet owned by the program
    #[account(mut, address = registry.wallet_key)]
    pub registry_wallet: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Slash<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    /// CHECK: The wallet where slashed funds are accumulated.
    #[account(mut, address = registry.wallet_key)]
    pub registry_wallet: AccountInfo<'info>,

    #[account(address = service.multisig)]
    pub user: Signer<'info>,
}

#[derive(Accounts)]
pub struct Deploy<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(mut)]
    pub service: Account<'info, ServiceAccount>,

    /// CHECK: service_owner
    #[account(address = service.service_owner)]
    pub service_owner: AccountInfo<'info>,

    #[account(mut)]
    pub registry_multisig: Account<'info, RegistryMultisig>,

    #[account(mut)]
    pub user: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ChangeMultisigPermission<'info> {
    #[account(mut)]
    pub registry: Account<'info, ServiceRegistry>,

    #[account(
        init_if_needed,
        payer = user,
        space = 8 + (MAX_MULTISIGS * PUBKEY_SIZE) + 8, // 8 bytes for Vec metadata + data for MAX_MULTISIGS * PUBKEY_SIZE  + 8 bytes Vec overhead
        seeds = [b"registry_multisig", registry.key().as_ref()],
        bump
    )]
    pub registry_multisig: Account<'info, RegistryMultisig>,

    #[account(mut, address = registry.owner)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DummyContextForAgentParam<'info> {
    pub agent_param_account: Account<'info, AgentParamAccount>,
}

#[derive(Accounts)]
pub struct DummyReadAgentInstances<'info> {
    #[account()]
    pub service_agent_instances_index: Account<'info, ServiceAgentInstancesIndex>,
}

#[derive(Accounts)]
pub struct DummyOperatorAgentInstanceAccount<'info> {
    #[account()]
    pub operator_agent_instance_account: Account<'info, OperatorAgentInstanceAccount>,
}

#[derive(Accounts)]
pub struct DummyServiceAgentInstanceAccount<'info> {
    #[account()]
    pub service_agent_instance_account: Account<'info, ServiceAgentInstanceAccount>,
}

#[derive(Accounts)]
pub struct DummyServiceAgentSlotCounterAccount<'info> {
    #[account()]
    pub service_agent_slot_counter_account: Account<'info, ServiceAgentSlotCounterAccount>,
}

#[derive(Accounts)]
pub struct DummyOperatorBondAccount<'info> {
    #[account()]
    pub agent_operator_bond_account: Account<'info, OperatorBondAccount>,
}
