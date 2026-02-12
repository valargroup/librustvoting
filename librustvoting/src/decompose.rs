/// Decompose voting weight into exactly 4 shares.
///
/// Per the protocol spec (§3.3.1), votes are split into exactly 4 shares
/// that are each encrypted as El Gamal ciphertexts. We first do a binary
/// decomposition, then distribute the resulting powers-of-2 across 4 buckets
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

    // If 4 or fewer bits set, pad with zeros to exactly 4
    if bits.len() <= 4 {
        bits.resize(4, 0);
        return bits;
    }

    // More than 4 bits set: distribute round-robin into 4 buckets
    let mut shares = vec![0u64; 4];
    for (i, &value) in bits.iter().enumerate() {
        shares[i % 4] += value;
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
        assert_eq!(shares.len(), 4);
        assert_eq!(shares.iter().sum::<u64>(), 1);
    }

    #[test]
    fn test_composite() {
        // 5 = 1 + 4 → [1, 4, 0, 0]
        let shares = decompose_weight(5);
        assert_eq!(shares.len(), 4);
        assert_eq!(shares.iter().sum::<u64>(), 5);
    }

    #[test]
    fn test_four_bits() {
        // 15 = 1 + 2 + 4 + 8 → exactly 4 shares
        let shares = decompose_weight(15);
        assert_eq!(shares, vec![1, 2, 4, 8]);
    }

    #[test]
    fn test_more_than_four_bits() {
        // 31 = 1 + 2 + 4 + 8 + 16 → 5 bits, bucketed into 4
        let shares = decompose_weight(31);
        assert_eq!(shares.len(), 4);
        assert_eq!(shares.iter().sum::<u64>(), 31);
        // Round-robin: bucket[0]=1+16=17, bucket[1]=2, bucket[2]=4, bucket[3]=8
        assert_eq!(shares, vec![17, 2, 4, 8]);
    }

    #[test]
    fn test_voting_weight() {
        // 142.50 ZEC = 14_250_000_000 zatoshi
        let weight = 14_250_000_000u64;
        let shares = decompose_weight(weight);
        assert_eq!(shares.len(), 4);
        assert_eq!(shares.iter().sum::<u64>(), weight);
    }

    #[test]
    fn test_real_balance() {
        // The balance from the simulator: 101768753
        let weight = 101_768_753u64;
        let shares = decompose_weight(weight);
        assert_eq!(shares.len(), 4);
        assert_eq!(shares.iter().sum::<u64>(), weight);
    }

    #[test]
    fn test_large_value() {
        let weight = u64::MAX;
        let shares = decompose_weight(weight);
        assert_eq!(shares.len(), 4);
        assert_eq!(shares.iter().sum::<u64>(), weight);
    }
}
