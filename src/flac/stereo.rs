use super::error::{Result, error};
use super::frame::ChannelAssignment;

pub(crate) fn restore(assignment: ChannelAssignment, channels: &mut [Vec<i64>]) -> Result<()> {
    match assignment {
        ChannelAssignment::Independent(_) => Ok(()),
        ChannelAssignment::LeftSide => {
            if channels.len() != 2 {
                return error("FLAC left-side frame does not have two channels");
            }
            let (left, side) = channels.split_at_mut(1);
            for (left, side) in left[0].iter().zip(side[0].iter_mut()) {
                *side = left.wrapping_sub(*side);
            }
            Ok(())
        }
        ChannelAssignment::RightSide => {
            if channels.len() != 2 {
                return error("FLAC right-side frame does not have two channels");
            }
            let (side, right) = channels.split_at_mut(1);
            for (side, right) in side[0].iter_mut().zip(right[0].iter()) {
                *side = side.wrapping_add(*right);
            }
            Ok(())
        }
        ChannelAssignment::MidSide => {
            if channels.len() != 2 {
                return error("FLAC mid-side frame does not have two channels");
            }
            let (mid, side) = channels.split_at_mut(1);
            for (mid, side) in mid[0].iter_mut().zip(side[0].iter_mut()) {
                let mid_sample = *mid;
                let side_sample = *side;
                // `mid` is floor((left + right) / 2); the side sample carries
                // the lost parity bit, so double `mid` and restore it before
                // the two exact half-sums below.
                let doubled = mid_sample.wrapping_shl(1) | (side_sample & 1);
                *mid = doubled.wrapping_add(side_sample) >> 1;
                *side = doubled.wrapping_sub(side_sample) >> 1;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::restore;
    use crate::flac::frame::ChannelAssignment;

    #[test]
    fn left_side() {
        let mut channels = vec![vec![2, 5, 83, 113], vec![7, 38, 142, 238]];
        restore(ChannelAssignment::LeftSide, &mut channels).unwrap();
        assert_eq!(
            channels,
            vec![vec![2, 5, 83, 113], vec![-5, -33, -59, -125]]
        );
    }

    #[test]
    fn right_side() {
        let mut channels = vec![vec![7, 38, 142, 238], vec![-5, -33, -59, -125]];
        restore(ChannelAssignment::RightSide, &mut channels).unwrap();
        assert_eq!(
            channels,
            vec![vec![2, 5, 83, 113], vec![-5, -33, -59, -125]]
        );
    }

    #[test]
    fn mid_side() {
        let mut channels = vec![vec![-2, -14, 12, -6], vec![7, 38, 142, 238]];
        restore(ChannelAssignment::MidSide, &mut channels).unwrap();
        assert_eq!(
            channels,
            vec![vec![2, 5, 83, 113], vec![-5, -33, -59, -125]]
        );
    }
}
