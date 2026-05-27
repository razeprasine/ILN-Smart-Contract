use soroban_sdk::{contracttype, Address, Env};

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub high_rep_threshold: u32,
    pub bonus_bps: u32,
    pub min_discount_rate_bps: u32,
    pub decay_rate_bps: u32,           // Basis points to decay per period (e.g., 50 = 0.5%)
    pub decay_period_ledgers: u64,     // Ledger count between decay applications
}

#[contracttype]
pub enum ConfigKey {
    Config,
    Admin,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Unauthorized,
    InvalidBonusBps,
    InvalidMinDiscountRate,
}

const MAX_BONUS_BPS: u32 = 500;

pub fn get_admin(env: &Env) -> Result<Address, ConfigError> {
    env.storage()
        .instance()
        .get(&ConfigKey::Admin)
        .ok_or(ConfigError::Unauthorized)
}

pub fn set_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&ConfigKey::Admin, admin);
}

pub fn get_config(env: &Env) -> Result<Config, ConfigError> {
    env.storage()
        .instance()
        .get(&ConfigKey::Config)
        .ok_or(ConfigError::Unauthorized)
}

pub fn set_config(env: &Env, config: &Config) -> Result<(), ConfigError> {
    if config.bonus_bps > MAX_BONUS_BPS {
        return Err(ConfigError::InvalidBonusBps);
    }
    if config.min_discount_rate_bps == 0 {
        return Err(ConfigError::InvalidMinDiscountRate);
    }
    env.storage().instance().set(&ConfigKey::Config, config);
    Ok(())
}

pub fn update_config(
    env: &Env,
    caller: &Address,
    high_rep_threshold: u32,
    bonus_bps: u32,
    min_discount_rate_bps: u32,
    decay_rate_bps: u32,
    decay_period_ledgers: u64,
) -> Result<(), ConfigError> {
    let admin = get_admin(env)?;
    caller.require_auth();
    if caller != &admin {
        return Err(ConfigError::Unauthorized);
    }

    let new_config = Config {
        high_rep_threshold,
        bonus_bps,
        min_discount_rate_bps,
        decay_rate_bps,
        decay_period_ledgers,
    };

    set_config(env, &new_config)
}
