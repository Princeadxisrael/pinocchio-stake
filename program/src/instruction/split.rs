use crate::{
    consts::PERPETUAL_NEW_WARMUP_COOLDOWN_RATE_EPOCH,
    error::StakeError,
    state::{
        bytes_to_u64, get_minimum_delegation, relocate_lamports, to_program_error,
        try_get_stake_state_mut, validate_split_amount, StakeAuthorize, StakeHistorySysvar,
        StakeStateV2,
    },
};
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};

use crate::state::utils::collect_signers;

// almost all native stake program processors accumulate every account signer
// they then defer all signer validation to functions on Meta or Authorized
// this results in an instruction interface that is much looser than the one documented
// to avoid breaking backwards compatibility, we do the same here
// in the future, we may decide to tighten the interface and break badly formed transactions

pub fn process_split(accounts: &[AccountInfo], split_lamports: u64) -> ProgramResult {
    let mut signers_arr = [Pubkey::default(); 32];
    let _signers = collect_signers(accounts, &mut signers_arr)?;

    let [source_stake_account_info, destination_stake_account_info, _rest @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    let clock = Clock::get()?;
    let stake_history = &StakeHistorySysvar(clock.epoch);

    let destination_data_len = destination_stake_account_info.data_len();
    if destination_data_len != StakeStateV2::size_of() {
        return Err(ProgramError::InvalidAccountData);
    }

    let source_lamport_balance = source_stake_account_info.lamports();
    let destination_lamport_balance = destination_stake_account_info.lamports();

    if split_lamports > source_lamport_balance {
        return Err(ProgramError::InsufficientFunds);
    }

    let mut source_stake_account: pinocchio::account_info::RefMut<'_, StakeStateV2> =
        try_get_stake_state_mut(source_stake_account_info)?;
    let mut dest_stake_account: pinocchio::account_info::RefMut<'_, StakeStateV2> =
        try_get_stake_state_mut(destination_stake_account_info)?;

    if let StakeStateV2::Uninitialized = *dest_stake_account {
        // we can split into this
    } else {
        return Err(ProgramError::InvalidAccountData);
    }

    match *source_stake_account {
        StakeStateV2::Stake(source_meta, mut source_stake, stake_flags) => {
            source_meta
                .authorized
                .check(&signers_arr, StakeAuthorize::Staker)
                .map_err(to_program_error)?;

            let minimum_delegation = get_minimum_delegation();

            let status = source_stake.delegation.stake_activating_and_deactivating(
                clock.epoch.to_be_bytes(),
                stake_history,
                PERPETUAL_NEW_WARMUP_COOLDOWN_RATE_EPOCH,
            );

            let is_active = bytes_to_u64(status.effective) > 0;

            // NOTE this function also internally summons Rent via syscall
            let validated_split_info = validate_split_amount(
                source_lamport_balance,
                destination_lamport_balance,
                split_lamports,
                &source_meta,
                destination_data_len,
                minimum_delegation,
                is_active,
            )?;

            // split the stake, subtract rent_exempt_balance unless
            // the destination account already has those lamports
            // in place.
            // this means that the new stake account will have a stake equivalent to
            // lamports minus rent_exempt_reserve if it starts out with a zero balance
            let (remaining_stake_delta, split_stake_amount) =
                if validated_split_info.source_remaining_balance == 0 {
                    // If split amount equals the full source stake (as implied by 0
                    // source_remaining_balance), the new split stake must equal the same
                    // amount, regardless of any current lamport balance in the split account.
                    // Since split accounts retain the state of their source account, this
                    // prevents any magic activation of stake by prefunding the split account.
                    //
                    // The new split stake also needs to ignore any positive delta between the
                    // original rent_exempt_reserve and the split_rent_exempt_reserve, in order
                    // to prevent magic activation of stake by splitting between accounts of
                    // different sizes.
                    let remaining_stake_delta = split_lamports
                        .saturating_sub(u64::from_le_bytes(source_meta.rent_exempt_reserve));
                    (remaining_stake_delta, remaining_stake_delta)
                } else {
                    // Otherwise, the new split stake should reflect the entire split
                    // requested, less any lamports needed to cover the
                    // split_rent_exempt_reserve.
                    if u64::from_le_bytes(source_stake.delegation.stake)
                        .saturating_sub(split_lamports)
                        < minimum_delegation
                    {
                        return Err(StakeError::InsufficientDelegation.into());
                    }

                    (
                        split_lamports,
                        split_lamports.saturating_sub(
                            validated_split_info
                                .destination_rent_exempt_reserve
                                .saturating_sub(destination_lamport_balance),
                        ),
                    )
                };

            if split_stake_amount < minimum_delegation {
                return Err(StakeError::InsufficientDelegation.into());
            }

            let destination_stake =
                source_stake.split(remaining_stake_delta, split_stake_amount)?;

            let mut destination_meta = source_meta;
            destination_meta.rent_exempt_reserve = validated_split_info
                .destination_rent_exempt_reserve
                .to_be_bytes();

            *source_stake_account = StakeStateV2::Stake(source_meta, source_stake, stake_flags);

            *dest_stake_account =
                StakeStateV2::Stake(destination_meta, destination_stake, stake_flags);
        }
        StakeStateV2::Initialized(source_meta) => {
            source_meta
                .authorized
                .check(&signers_arr, StakeAuthorize::Staker)
                .map_err(to_program_error)?;

            // NOTE this function also internally summons Rent via syscall
            let validated_split_info = validate_split_amount(
                source_lamport_balance,
                destination_lamport_balance,
                split_lamports,
                &source_meta,
                destination_data_len,
                0,     // additional_required_lamports
                false, // is_active
            )?;

            let mut destination_meta = source_meta;
            destination_meta.rent_exempt_reserve = validated_split_info
                .destination_rent_exempt_reserve
                .to_le_bytes();

            *dest_stake_account = StakeStateV2::Initialized(destination_meta);
        }
        StakeStateV2::Uninitialized => {
            if !source_stake_account_info.is_signer() {
                return Err(ProgramError::MissingRequiredSignature);
            }
        }
        _ => return Err(ProgramError::InvalidAccountData),
    }
    if split_lamports == source_lamport_balance {
        *source_stake_account = StakeStateV2::Uninitialized;
    }
    relocate_lamports(
        source_stake_account_info,
        destination_stake_account_info,
        split_lamports,
    )?;

    Ok(())
}
