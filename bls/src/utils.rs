use ark_ff::BigInteger768;

/// Transforms a vector of bytes into the corresponding vector of bits (booleans).
/// The output is in the same format as the input (e.g. if the input is in big-endian, the output
/// will also be in big-endian).
pub fn bytes_to_bits(bytes: &[u8]) -> Vec<bool> {
    let mut bits = vec![];

    for byte in bytes {
        for j in (0..8).rev() {
            bits.push((byte >> j) & 1 == 1);
        }
    }

    bits
}

/// Creates a BigInteger from an array of bytes in big-endian format.
pub fn big_int_from_bytes_be<R: std::io::Read>(reader: &mut R) -> BigInteger768 {
    let mut res = [0u64; 12];

    for num in res.iter_mut().rev() {
        let mut bytes = [0u8; 8];
        reader.read_exact(&mut bytes).unwrap();
        *num = u64::from_be_bytes(bytes);
    }

    BigInteger768::new(res)
}

/// Transforms a vector of little endian bits into a u8.
pub fn byte_from_le_bits(bits: &[bool]) -> u8 {
    assert!(bits.len() <= 8);

    let mut byte: u8 = 0;
    let mut base = 1;

    for bit in bits {
        if *bit {
            byte += base;
        }
        base = base.wrapping_mul(2);
    }

    byte
}

/// Transforms a vector of little endian bits into a vector of u8's in big-endian.
pub fn bytes_be_from_le_bits(bits: &[bool]) -> Vec<u8> {
    assert_eq!(bits.len() % 8, 0);

    let mut bytes = vec![];

    for i in 0..bits.len() / 8 {
        bytes.push(byte_from_le_bits(&bits[i * 8..(i + 1) * 8]));
    }

    bytes.reverse();

    bytes
}

/// Transforms a u8 into a vector of little endian bits.
pub fn byte_to_le_bits(mut byte: u8) -> Vec<bool> {
    let mut bits = vec![];

    for _ in 0..8 {
        bits.push(byte % 2 != 0);
        byte >>= 1;
    }

    bits
}

/// Transforms a vector of big endian bits into a u8.
pub fn byte_from_be_bits(bits: &[bool]) -> u8 {
    assert!(bits.len() <= 8);

    let mut byte: u8 = 0;
    let mut base = 1;

    for bit in bits.iter().rev() {
        if *bit {
            byte += base;
        }
        base = base.wrapping_mul(2);
    }

    byte
}
