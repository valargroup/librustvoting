/// Decompose voting weight into exactly 5 shares.
///
/// Per the protocol spec (§3.3.1), votes are split into exactly 5 shares
/// that are each encrypted as El Gamal ciphertexts. We first do a binary
/// decomposition, then distribute the resulting powers-of-2 across 5 buckets
/// using round-robin assignment, summing within each bucket.
///
/// Returns empty vec for weight=0.
pub fn decompose_weight(weight: u64) -> Vec<u64> {
    if weight == 0 {
        return vec![];
    }

    // Binary decompose into powers of 2
    let mut bits = Vec::new();
    let mut remaining = weight;
    let mut bit_position = 0u32;

    while remaining > 0 {
        if remaining & 1 == 1 {
            bits.push(1u64 << bit_position);
        }
        remaining >>= 1;
        bit_position += 1;
    }

    // If 5 or fewer bits set, pad with zeros to exactly 5
    if bits.len() <= 5 {
        bits.resize(5, 0);
        return bits;
    }

    // More than 5 bits set: distribute round-robin into 5 buckets
    let mut shares = vec![0u64; 5];
    for (i, &value) in bits.iter().enumerate() {
        shares[i % 5] += value;
    }

    shares
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero() {
        assert_eq!(decompose_weight(0), Vec::<u64>::new());
    }

    #[test]
    fn test_power_of_two() {
        let shares = decompose_weight(1);
        assert_eq!(shares.len(), 5);
        assert_eq!(shares.iter().sum::<u64>(), 1);
    }

    #[test]
    fn test_composite() {
        // 5 = 1 + 4 → [1, 4, 0, 0, 0]
        let shares = decompose_weight(5);
        assert_eq!(shares.len(), 5);
        assert_eq!(shares.iter().sum::<u64>(), 5);
    }

    #[test]
    fn test_five_bits() {
        // 31 = 1 + 2 + 4 + 8 + 16 → exactly 5 shares
        let shares = decompose_weight(31);
        assert_eq!(shares, vec![1, 2, 4, 8, 16]);
    }

    #[test]
    fn test_more_than_five_bits() {
        // 63 = 1 + 2 + 4 + 8 + 16 + 32 → 6 bits, bucketed into 5
        let shares = decompose_weight(63);
        assert_eq!(shares.len(), 5);
        assert_eq!(shares.iter().sum::<u64>(), 63);
        // Round-robin: bucket[0]=1+32=33, bucket[1]=2, bucket[2]=4, bucket[3]=8, bucket[4]=16
        assert_eq!(shares, vec![33, 2, 4, 8, 16]);
    }

    #[test]
    fn test_voting_weight() {
        // 142.50 ZEC = 14_250_000_000 zatoshi
        let weight = 14_250_000_000u64;
        let shares = decompose_weight(weight);
        assert_eq!(shares.len(), 5);
        assert_eq!(shares.iter().sum::<u64>(), weight);
    }

    #[test]
    fn test_real_balance() {
        // The balance from the simulator: 101768753
        let weight = 101_768_753u64;
        let shares = decompose_weight(weight);
        assert_eq!(shares.len(), 5);
        assert_eq!(shares.iter().sum::<u64>(), weight);
    }

    #[test]
    fn test_large_value() {
        let weight = u64::MAX;
        let shares = decompose_weight(weight);
        assert_eq!(shares.len(), 5);
        assert_eq!(shares.iter().sum::<u64>(), weight);
    }
}
