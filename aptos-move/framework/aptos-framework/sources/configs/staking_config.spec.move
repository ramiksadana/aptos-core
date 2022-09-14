spec aptos_framework::staking_config {
    spec module {
        use aptos_framework::chain_status;
        invariant chain_status::is_operating() ==> exists<StakingConfig>(@aptos_framework);
    }
    spec StakingConfig {
        // `rewards_rate` which is the numerator is limited to be less than 100 in order to avoid the arithmetic
        // overflow in the rewards calculation. `rewards_rate_denominator` can be adjusted to get the desired rewards
        // rate (rewards_rate / rewards_rate_denominator).
        invariant rewards_rate < 100;
    }
}
