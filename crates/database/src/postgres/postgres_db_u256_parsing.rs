use bytes::BufMut;
use ethereum_consensus::primitives::U256;

use tokio_postgres::types::{FromSql, ToSql};

#[derive(Debug, Clone)]
pub struct PostgresNumeric(pub U256);

impl From<U256> for PostgresNumeric {
    fn from(value: U256) -> Self {
        PostgresNumeric(value)
    }
}

impl From<PostgresNumeric> for U256 {
    fn from(value: PostgresNumeric) -> Self {
        value.0
    }
}

const NBASE: u64 = 10000;

/// Implements the `FromSql` trait for `PostgresNumeric`.
/// We need a slightly generalized implementation since postgres
/// optimizes some stuff when storing so that the bytes we provide are not stored
/// in the exact way we provide them.
/// E.g. some tests have shown that 1000_000_000_000_000_000_000_000_000 is stored as
/// [0, 1, 0, 6, 0, 0, 0, 0, 3, 232]
/// sign and dscale are still not used

impl<'a> FromSql<'a> for PostgresNumeric {
    fn from_sql(
        _: &tokio_postgres::types::Type,
        raw: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let n_base = U256::from(NBASE);
        let mut offset = 0;

        // Function to read two bytes and advance the offset
        let read_two_bytes = |raw: &[u8], offset: &mut usize| -> std::io::Result<u16> {
            if raw.len() < *offset + 2 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Not enough bytes to read",
                ))
            }
            let value = u16::from_be_bytes([raw[*offset], raw[*offset + 1]]);
            *offset += 2;
            Ok(value)
        };

        let num_groups = read_two_bytes(raw, &mut offset)?;
        let weight = read_two_bytes(raw, &mut offset)?;
        let _sign = read_two_bytes(raw, &mut offset)?;
        let _dscale = read_two_bytes(raw, &mut offset)?;

        let mut value = U256::from(0);
        for _ in 0..num_groups {
            value = value * n_base + U256::from(read_two_bytes(raw, &mut offset)?);
        }

        value *= n_base.pow(U256::from((weight + 1).saturating_sub(num_groups)));

        Ok(PostgresNumeric(value))
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        matches!(*ty, tokio_postgres::types::Type::NUMERIC)
    }
}

/// Implements the `ToSql` trait for `PostgresNumeric`.
/// Some things to note about this implementation:
/// - Assumes positive numbers
/// - Assumes scale of 0
/// - Assumes weight of 0
/// As such not generalized, but good enough for our purposes
/// Allows for MAX_GROUP_COUNT digit groups, each group is a value between 0 and 9999
/// so the maximum value is NBASE^MAX_GROUP_COUNT - 1
/// with MAX_GROUP_COUNT = 32 this should be plenty to store any U256
/// Obviously not sufficient for arbitrary precision.
impl ToSql for PostgresNumeric {
    fn to_sql(
        &self,
        _: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
        const MAX_GROUP_COUNT: usize = 32;
        let divisor = U256::from(NBASE);
        let mut temp = self.0;
        let mut digits = [0i16; MAX_GROUP_COUNT];
        let mut num_digits = 0;

        while temp != U256::from(0) {
            let (quotient, remainder) = temp.div_rem(divisor);
            digits[num_digits] = remainder.as_limbs()[0] as i16;
            num_digits += 1;
            temp = quotient;
        }

        if num_digits == 0 {
            num_digits = 1; // Ensure at least one digit
        }
        let weight = (num_digits as i16).saturating_sub(1);

        // Reserve bytes
        out.reserve(8 + num_digits * 2);

        // Number of groups
        out.put_u16(num_digits as u16);
        // Weight of first group
        out.put_i16(weight);
        // Sign (assuming positive numbers)
        out.put_u16(0x0000);
        // DScale (assuming scale of 0)
        out.put_u16(0);

        for digit in digits.iter().take(num_digits).rev() {
            out.put_i16(*digit);
        }

        Ok(tokio_postgres::types::IsNull::No)
    }

    fn accepts(_: &tokio_postgres::types::Type) -> bool {
        true
    }

    tokio_postgres::types::to_sql_checked!();
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::postgres_db_u256_parsing::PostgresNumeric;

    use ethereum_consensus::primitives::U256;

    fn get_values() -> Vec<U256> {
        vec![
            U256::from(0),
            U256::from(1),
            U256::from(12345678),
            U256::from(12088888526885516_u64),
            U256::from(u64::MAX),
            U256::from_str_radix("1000_000_000_000_000_000", 10).unwrap(),
            U256::from_str_radix("1000_000_000_000_000_000_000", 10).unwrap(),
            U256::from_str_radix(
                "1000_000_000_000_000_000_000_000_000_000_000_000_000_000_000",
                10,
            )
            .unwrap(),
            U256::MAX,
        ]
    }

    #[test]
    fn test_to_sql_manual_reconstruction() {
        for value in get_values().into_iter() {
            let mut bytes = bytes::BytesMut::new();
            let result = PostgresNumeric::from(value)
                .to_sql(&tokio_postgres::types::Type::NUMERIC, &mut bytes);
            assert!(result.is_ok());
            let digits = &bytes[8..]
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<u16>>();

            let reconstructed_value = digits
                .iter()
                .fold(U256::from(0), |acc, digit| acc * U256::from(NBASE) + U256::from(*digit));

            assert_eq!(value, reconstructed_value);
        }
    }

    #[test]
    #[ignore]
    fn test_to_sql_from_sql() {
        for value in get_values().into_iter() {
            let mut bytes = bytes::BytesMut::new();
            let result = PostgresNumeric::from(value)
                .to_sql(&tokio_postgres::types::Type::NUMERIC, &mut bytes);
            assert!(result.is_ok());
            let reconstructed_value =
                PostgresNumeric::from_sql(&tokio_postgres::types::Type::NUMERIC, &bytes[..])
                    .unwrap();

            assert_eq!(value, reconstructed_value.0);
        }
    }
}
