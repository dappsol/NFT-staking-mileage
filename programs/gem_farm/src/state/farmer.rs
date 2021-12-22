use anchor_lang::prelude::*;
use gem_common::{errors::ErrorCode, *};

use crate::number128::Number128;
use crate::state::{FixedRateReward, FixedRateRewardTier, FixedRateSchedule};

#[repr(C)]
#[derive(Debug, Copy, Clone, AnchorSerialize, AnchorDeserialize, PartialEq)]
pub enum FarmerState {
    Unstaked,
    Staked,
    PendingCooldown,
}

#[repr(C)]
#[account]
#[derive(Debug)]
pub struct Farmer {
    pub farm: Pubkey,

    // the identity of the farmer = their public key
    pub identity: Pubkey,

    // vault storing all of the farmer's gems
    pub vault: Pubkey,

    pub state: FarmerState,

    // total number of gems at the time when the vault is locked
    pub gems_staked: u64,

    pub min_staking_ends_ts: u64,

    pub cooldown_ends_ts: u64,

    // --------------------------------------- rewards
    pub reward_a: FarmerReward,

    pub reward_b: FarmerReward,
}

impl Farmer {
    pub fn begin_staking(
        &mut self,
        min_staking_period_sec: u64,
        now_ts: u64,
        gems_in_vault: u64,
    ) -> ProgramResult {
        self.state = FarmerState::Staked;
        self.gems_staked = gems_in_vault;
        self.min_staking_ends_ts = now_ts.try_add(min_staking_period_sec)?;
        self.cooldown_ends_ts = 0; //zero it out in case it was set before

        // begin a new staking cycle (variable rewards will simply ignore this)
        self.reward_a.fixed_rate.reset_staking_cycle();
        self.reward_b.fixed_rate.reset_staking_cycle();

        Ok(())
    }

    pub fn end_staking_begin_cooldown(
        &mut self,
        now_ts: u64,
        cooldown_period_sec: u64,
    ) -> Result<u64, ProgramError> {
        if !self.can_end_staking(now_ts) {
            return Err(ErrorCode::MinStakingNotPassed.into());
        }

        self.state = FarmerState::PendingCooldown;
        let gems_unstaked = self.gems_staked;
        self.gems_staked = 0; //no rewards will accrue during cooldown period
        self.cooldown_ends_ts = now_ts.try_add(cooldown_period_sec)?;

        msg!(
            "{} gems now cooling down for {}",
            gems_unstaked,
            self.identity
        );
        Ok(gems_unstaked)
    }

    pub fn end_cooldown(&mut self, now_ts: u64) -> ProgramResult {
        if !self.can_end_cooldown(now_ts) {
            return Err(ErrorCode::CooldownNotPassed.into());
        }

        self.state = FarmerState::Unstaked;
        // zero everything out
        self.gems_staked = 0;
        self.min_staking_ends_ts = 0;
        self.cooldown_ends_ts = 0;

        msg!(
            "gems now unstaked and available for withdrawal for {}",
            self.identity
        );
        Ok(())
    }

    fn can_end_staking(&self, now_ts: u64) -> bool {
        now_ts >= self.min_staking_ends_ts
    }

    fn can_end_cooldown(&self, now_ts: u64) -> bool {
        now_ts >= self.cooldown_ends_ts
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, AnchorSerialize, AnchorDeserialize)]
pub struct FarmerReward {
    // total, not per gem
    pub paid_out_reward: u64,

    // total, not per gem
    pub accrued_reward: u64,

    pub variable_rate: FarmerVariableRateReward,

    pub fixed_rate: FarmerFixedRateReward,
}

impl FarmerReward {
    pub fn outstanding_reward(&self) -> Result<u64, ProgramError> {
        self.accrued_reward.try_sub(self.paid_out_reward)
    }

    pub fn claim_reward(&mut self, pot_balance: u64) -> Result<u64, ProgramError> {
        let outstanding = self.outstanding_reward()?;
        let to_claim = std::cmp::min(outstanding, pot_balance);

        self.paid_out_reward.try_add_assign(to_claim)?;

        Ok(to_claim)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, AnchorSerialize, AnchorDeserialize)]
pub struct FarmerVariableRateReward {
    // used to keep track of how much of the variable reward has been updated for this farmer
    // (read more in variable rate config)
    pub last_recorded_accrued_reward_per_gem: Number128,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, AnchorSerialize, AnchorDeserialize)]
pub struct FarmerFixedRateReward {
    pub begin_staking_ts: u64,

    pub last_updated_ts: u64,

    pub promised_schedule: FixedRateSchedule,

    pub promised_duration: u64,

    pub reward_counted_as_accrued: u64,
}

impl FarmerFixedRateReward {
    pub fn graduation_time(&self) -> Result<u64, ProgramError> {
        self.begin_staking_ts.try_add(self.promised_duration)
    }

    // pub fn capped_accrued_duration(&self, now_ts: u64) -> Result<u64, ProgramError> {
    //     let upper_bound_ts = std::cmp::min(now_ts, self.graduation_time()?);
    //     upper_bound_ts.try_sub(self.begin_staking_ts)
    // }

    // pub fn unaccrued_duration(&self) -> Result<u64, ProgramError> {
    //     self.begin_staking_ts
    //         .try_add(self.promised_duration)?
    //         .try_sub(self.last_updated_ts)
    // }

    pub fn is_graduation_time(&self, now_ts: u64) -> Result<bool, ProgramError> {
        Ok(now_ts >= self.graduation_time()?)
    }

    pub fn lower_bound_ts(&self) -> u64 {
        std::cmp::max(self.begin_staking_ts, self.last_updated_ts)
    }

    pub fn upper_bound_ts(&self, now_ts: u64) -> Result<u64, ProgramError> {
        Ok(std::cmp::min(now_ts, self.graduation_time()?))
    }

    pub fn voided_reward(&self, gems: u64) -> Result<u64, ProgramError> {
        let start_from = self.last_updated_ts.try_sub(self.begin_staking_ts)?;
        let end_at = self.graduation_time()?.try_sub(self.begin_staking_ts)?;
        self.promised_schedule.calc_amount(start_from, end_at, gems)
    }

    pub fn newly_accrued_reward(&self, now_ts: u64, gems: u64) -> Result<u64, ProgramError> {
        let start_from = self.last_updated_ts.try_sub(self.begin_staking_ts)?;
        let end_at = self
            .upper_bound_ts(now_ts)?
            .try_sub(self.begin_staking_ts)?;
        self.promised_schedule.calc_amount(start_from, end_at, gems)
    }
}
