use {
    crate::{
        error::TokenError,
        extension::{Extension, ExtensionType},
        pod::*,
    },
    bytemuck::{Pod, Zeroable},
    solana_program::{clock::Epoch, entrypoint::ProgramResult},
    std::{cmp, convert::TryFrom},
};

/// Transfer fee extension instructions
pub mod instruction;

/// Transfer fee extension processor
pub mod processor;

/// Maximum possible fee in basis points is 100%, aka 10_000 basis points
pub const MAX_FEE_BASIS_POINTS: u16 = 10_000;
const ONE_IN_BASIS_POINTS: u128 = MAX_FEE_BASIS_POINTS as u128;

/// Transfer fee information
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TransferFee {
    /// First epoch where the transfer fee takes effect
    pub epoch: PodU64, // Epoch,
    /// Maximum fee assessed on transfers, expressed as an amount of tokens
    pub maximum_fee: PodU64,
    /// Amount of transfer collected as fees, expressed as basis points of the
    /// transfer amount, ie. increments of 0.01%
    pub transfer_fee_basis_points: PodU16,
}
impl TransferFee {
    /// Calculate the transfer fee
    pub fn calculate_fee(&self, pre_fee_amount: u64) -> Option<u64> {
        let transfer_fee_basis_points = u16::from(self.transfer_fee_basis_points) as u128;
        if transfer_fee_basis_points == 0 || pre_fee_amount == 0 {
            Some(0)
        } else {
            let numerator = (pre_fee_amount as u128).checked_mul(transfer_fee_basis_points)?;
            let mut raw_fee = numerator.checked_div(ONE_IN_BASIS_POINTS)?;
            let remainder = numerator.checked_rem(ONE_IN_BASIS_POINTS)?;
            if remainder > 0 {
                raw_fee = raw_fee.checked_add(1)?;
            }
            // guaranteed to be ok
            let raw_fee = u64::try_from(raw_fee).ok()?;
            Some(cmp::min(raw_fee, u64::from(self.maximum_fee)))
        }
    }

    /// Calculate the fee that would produce the given output
    pub fn calculate_inverse_fee(&self, post_fee_amount: u64) -> Option<u64> {
        let transfer_fee_basis_points = u16::from(self.transfer_fee_basis_points) as u128;
        if transfer_fee_basis_points == 0 {
            Some(0)
        } else if transfer_fee_basis_points == ONE_IN_BASIS_POINTS {
            None // if transfer fee is total amount, we can't ever figure out the output
        } else if post_fee_amount == 0 {
            Some(cmp::min(1, u64::from(self.maximum_fee)))
        } else {
            let post_fee_amount = post_fee_amount as u128;
            let denominator = ONE_IN_BASIS_POINTS.checked_sub(transfer_fee_basis_points)?;
            // try without remainder in out amount
            let numerator = post_fee_amount.checked_mul(ONE_IN_BASIS_POINTS)?;
            let pre_fee_amount = numerator.checked_div(denominator)?;

            // with this amount in, does the fee calculation produce a remainder?
            // if so, probably not the right one
            let check_numerator = pre_fee_amount.checked_mul(transfer_fee_basis_points)?;
            let check_remainder = check_numerator.checked_rem(ONE_IN_BASIS_POINTS)?;
            let raw_fee = if check_remainder == 0 {
                check_numerator.checked_div(ONE_IN_BASIS_POINTS)?
            } else {
                // try with remainder
                let numerator = post_fee_amount
                    .checked_add(1)?
                    .checked_mul(ONE_IN_BASIS_POINTS)?;
                let pre_fee_amount = numerator.checked_div(denominator)?;
                pre_fee_amount.checked_sub(post_fee_amount)?
            };
            // the division and rounding can go above u64::MAX, so protect
            let fee = if raw_fee > u64::MAX as u128 {
                u64::MAX
            } else {
                u64::try_from(raw_fee).ok()?
            };
            Some(cmp::min(fee, u64::from(self.maximum_fee)))
        }
    }
}

/// Transfer fee extension data for mints.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TransferFeeConfig {
    /// Optional authority to set the fee
    pub transfer_fee_config_authority: OptionalNonZeroPubkey,
    /// Withdraw from mint instructions must be signed by this key
    pub withdraw_withheld_authority: OptionalNonZeroPubkey,
    /// Withheld transfer fee tokens that have been moved to the mint for withdrawal
    pub withheld_amount: PodU64,
    /// Older transfer fee, used if the current epoch < new_transfer_fee.epoch
    pub older_transfer_fee: TransferFee,
    /// Newer transfer fee, used if the current epoch >= new_transfer_fee.epoch
    pub newer_transfer_fee: TransferFee,
}
impl TransferFeeConfig {
    /// Get the fee for the given epoch
    pub fn get_epoch_fee(&self, epoch: Epoch) -> &TransferFee {
        if epoch >= self.newer_transfer_fee.epoch.into() {
            &self.newer_transfer_fee
        } else {
            &self.older_transfer_fee
        }
    }
    /// Calculate the fee for the given epoch and input amount
    pub fn calculate_epoch_fee(&self, epoch: Epoch, pre_fee_amount: u64) -> Option<u64> {
        self.get_epoch_fee(epoch).calculate_fee(pre_fee_amount)
    }
    /// Calculate the fee for the given epoch and output amount
    pub fn calculate_inverse_epoch_fee(&self, epoch: Epoch, post_fee_amount: u64) -> Option<u64> {
        self.get_epoch_fee(epoch)
            .calculate_inverse_fee(post_fee_amount)
    }
}
impl Extension for TransferFeeConfig {
    const TYPE: ExtensionType = ExtensionType::TransferFeeConfig;
}

/// Transfer fee extension data for accounts.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct TransferFeeAmount {
    /// Amount withheld during transfers, to be harvested to the mint
    pub withheld_amount: PodU64,
}
impl TransferFeeAmount {
    /// Check if the extension is in a closable state
    pub fn closable(&self) -> ProgramResult {
        if self.withheld_amount == 0.into() {
            Ok(())
        } else {
            Err(TokenError::AccountHasWithheldTransferFees.into())
        }
    }
}
impl Extension for TransferFeeAmount {
    const TYPE: ExtensionType = ExtensionType::TransferFeeAmount;
}

#[cfg(test)]
pub(crate) mod test {
    use {super::*, proptest::prelude::*, solana_program::pubkey::Pubkey, std::convert::TryFrom};

    const NEWER_EPOCH: u64 = 100;
    const OLDER_EPOCH: u64 = 1;

    pub(crate) fn test_transfer_fee_config() -> TransferFeeConfig {
        TransferFeeConfig {
            transfer_fee_config_authority: OptionalNonZeroPubkey::try_from(Some(Pubkey::new(
                &[10; 32],
            )))
            .unwrap(),
            withdraw_withheld_authority: OptionalNonZeroPubkey::try_from(Some(Pubkey::new(
                &[11; 32],
            )))
            .unwrap(),
            withheld_amount: PodU64::from(u64::MAX),
            older_transfer_fee: TransferFee {
                epoch: PodU64::from(OLDER_EPOCH),
                maximum_fee: PodU64::from(10),
                transfer_fee_basis_points: PodU16::from(100),
            },
            newer_transfer_fee: TransferFee {
                epoch: PodU64::from(NEWER_EPOCH),
                maximum_fee: PodU64::from(5_000),
                transfer_fee_basis_points: PodU16::from(1),
            },
        }
    }

    #[test]
    fn epoch_fee() {
        let transfer_fee_config = test_transfer_fee_config();
        // during epoch 100 and after, use newer transfer fee
        assert_eq!(
            transfer_fee_config.get_epoch_fee(NEWER_EPOCH).epoch,
            NEWER_EPOCH.into()
        );
        assert_eq!(
            transfer_fee_config.get_epoch_fee(NEWER_EPOCH + 1).epoch,
            NEWER_EPOCH.into()
        );
        assert_eq!(
            transfer_fee_config.get_epoch_fee(u64::MAX).epoch,
            NEWER_EPOCH.into()
        );
        // before that, use older transfer fee
        assert_eq!(
            transfer_fee_config.get_epoch_fee(NEWER_EPOCH - 1).epoch,
            OLDER_EPOCH.into()
        );
        assert_eq!(
            transfer_fee_config.get_epoch_fee(OLDER_EPOCH).epoch,
            OLDER_EPOCH.into()
        );
        assert_eq!(
            transfer_fee_config.get_epoch_fee(OLDER_EPOCH + 1).epoch,
            OLDER_EPOCH.into()
        );
    }

    #[test]
    fn calculate_fee_max() {
        let one = u64::try_from(ONE_IN_BASIS_POINTS).unwrap();
        let transfer_fee = TransferFee {
            epoch: PodU64::from(0),
            maximum_fee: PodU64::from(5_000),
            transfer_fee_basis_points: PodU16::from(1),
        };
        let maximum_fee = u64::from(transfer_fee.maximum_fee);
        // hit maximum fee
        assert_eq!(maximum_fee, transfer_fee.calculate_fee(u64::MAX).unwrap());
        // at exactly the max
        assert_eq!(
            maximum_fee,
            transfer_fee.calculate_fee(maximum_fee * one).unwrap()
        );
        // one token above, normally rounds up, but we're at the max
        assert_eq!(
            maximum_fee,
            transfer_fee.calculate_fee(maximum_fee * one + 1).unwrap()
        );
        // one token below, rounds up to the max
        assert_eq!(
            maximum_fee,
            transfer_fee.calculate_fee(maximum_fee * one - 1).unwrap()
        );
    }

    #[test]
    fn calculate_fee_min() {
        let one = u64::try_from(ONE_IN_BASIS_POINTS).unwrap();
        let transfer_fee = TransferFee {
            epoch: PodU64::from(0),
            maximum_fee: PodU64::from(5_000),
            transfer_fee_basis_points: PodU16::from(1),
        };
        let minimum_fee = 1;
        // hit minimum fee even with 1 token
        assert_eq!(minimum_fee, transfer_fee.calculate_fee(1).unwrap());
        // still minimum at 2 tokens
        assert_eq!(minimum_fee, transfer_fee.calculate_fee(2).unwrap());
        // still minimum at 10_000 tokens
        assert_eq!(minimum_fee, transfer_fee.calculate_fee(one).unwrap());
        // 2 token fee at 10_001
        assert_eq!(
            minimum_fee + 1,
            transfer_fee.calculate_fee(one + 1).unwrap()
        );
        // zero is always zero
        assert_eq!(0, transfer_fee.calculate_fee(0).unwrap());
    }

    #[test]
    fn calculate_fee_zero() {
        let one = u64::try_from(ONE_IN_BASIS_POINTS).unwrap();
        let transfer_fee = TransferFee {
            epoch: PodU64::from(0),
            maximum_fee: PodU64::from(u64::MAX),
            transfer_fee_basis_points: PodU16::from(0),
        };
        // always zero fee
        assert_eq!(0, transfer_fee.calculate_fee(0).unwrap());
        assert_eq!(0, transfer_fee.calculate_fee(u64::MAX).unwrap());
        assert_eq!(0, transfer_fee.calculate_fee(1).unwrap());
        assert_eq!(0, transfer_fee.calculate_fee(one).unwrap());

        let transfer_fee = TransferFee {
            epoch: PodU64::from(0),
            maximum_fee: PodU64::from(0),
            transfer_fee_basis_points: PodU16::from(MAX_FEE_BASIS_POINTS),
        };
        // always zero fee
        assert_eq!(0, transfer_fee.calculate_fee(0).unwrap());
        assert_eq!(0, transfer_fee.calculate_fee(u64::MAX).unwrap());
        assert_eq!(0, transfer_fee.calculate_fee(1).unwrap());
        assert_eq!(0, transfer_fee.calculate_fee(one).unwrap());
    }

    #[test]
    fn calculate_fee_exact_out_max() {
        let one = u64::try_from(ONE_IN_BASIS_POINTS).unwrap();
        let transfer_fee = TransferFee {
            epoch: PodU64::from(0),
            maximum_fee: PodU64::from(5_000),
            transfer_fee_basis_points: PodU16::from(1),
        };
        let maximum_fee = u64::from(transfer_fee.maximum_fee);
        // hit maximum fee
        assert_eq!(
            maximum_fee,
            transfer_fee.calculate_inverse_fee(u64::MAX).unwrap()
        );
        // at exactly the max
        assert_eq!(
            maximum_fee,
            transfer_fee
                .calculate_inverse_fee(maximum_fee * one - maximum_fee)
                .unwrap()
        );
        // one token above, normally rounds up, but we're at the max
        assert_eq!(
            maximum_fee,
            transfer_fee
                .calculate_inverse_fee(maximum_fee * one - maximum_fee + 1)
                .unwrap()
        );
        // one token below, rounds up to the max
        assert_eq!(
            maximum_fee,
            transfer_fee
                .calculate_inverse_fee(maximum_fee * one - maximum_fee - 1)
                .unwrap()
        );
    }

    #[test]
    fn calculate_fee_exact_out_min() {
        let one = u64::try_from(ONE_IN_BASIS_POINTS).unwrap();
        let transfer_fee = TransferFee {
            epoch: PodU64::from(0),
            maximum_fee: PodU64::from(5_000),
            transfer_fee_basis_points: PodU16::from(1),
        };
        let minimum_fee = 1;
        // hit minimum fee even with 1 token
        assert_eq!(minimum_fee, transfer_fee.calculate_inverse_fee(1).unwrap());
        // still minimum at 2 tokens
        assert_eq!(minimum_fee, transfer_fee.calculate_inverse_fee(2).unwrap());
        // still minimum at 9_999 tokens
        assert_eq!(
            minimum_fee,
            transfer_fee.calculate_inverse_fee(one - 1).unwrap()
        );
        // 2 token fee at 10_000
        assert_eq!(
            minimum_fee + 1,
            transfer_fee.calculate_inverse_fee(one).unwrap()
        );
        // zero is 1 token
        assert_eq!(minimum_fee, transfer_fee.calculate_inverse_fee(0).unwrap());
    }

    proptest! {
        #[test]
        fn round_trip_fee_calculation(
            transfer_fee_basis_points in 0u16..MAX_FEE_BASIS_POINTS,
            maximum_fee in u64::MIN..=u64::MAX,
            amount_in in 0..=u64::MAX
        ) {
            let transfer_fee = TransferFee {
                epoch: PodU64::from(0),
                maximum_fee: PodU64::from(maximum_fee),
                transfer_fee_basis_points: PodU16::from(transfer_fee_basis_points),
            };
            let fee = transfer_fee.calculate_fee(amount_in).unwrap();
            let amount_out = amount_in.checked_sub(fee).unwrap();
            let fee_exact_out = transfer_fee.calculate_inverse_fee(amount_out).unwrap();
            let diff = if fee > fee_exact_out {
                fee - fee_exact_out
            } else {
                fee_exact_out - fee
            };
            // We lose precision with every division by 10000, so for huge amounts,
            // the difference can be in the hundreds. This comes out to less than
            // 1 / 10^15
            let one = MAX_FEE_BASIS_POINTS as u64;
            let precision = amount_in / one / one / one;
            assert!(diff < precision, "diff is {} for precision {}", diff, precision);
        }
    }
}
