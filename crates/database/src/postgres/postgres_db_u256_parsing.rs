
use bytes::BufMut;
use ethereum_consensus::primitives::U256;
use std::io::Read;

use tokio_postgres::types::{ToSql, FromSql};

#[derive(Debug, Clone)]
pub struct PostgresNumeric(U256);

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

fn read_two_bytes(cursor: &mut std::io::Cursor<&[u8]>) -> std::io::Result<[u8; 2]> {
    let mut result = [0; 2];
    cursor.read_exact(&mut result)?;
    Ok(result)
}

impl <'a>FromSql<'a> for PostgresNumeric {
    fn from_sql(_: &tokio_postgres::types::Type, raw: &[u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let mut raw = std::io::Cursor::new(raw);
        let num_groups = u16::from_be_bytes(read_two_bytes(&mut raw)?);

        // We don't use all these, but we might at some point?
        // In any case don't remove these because they advance the cursor
        let _weight = i16::from_be_bytes(read_two_bytes(&mut raw)?);
        let _sign = u16::from_be_bytes(read_two_bytes(&mut raw)?);
        let _scale = u16::from_be_bytes(read_two_bytes(&mut raw)?);

        let mut value = U256::from(0);

        for _ in 0..num_groups {
            let group = u16::from_be_bytes(read_two_bytes(&mut raw)?);
            value *= U256::from(10000_u64);
            value += U256::from(group);
        }

        Ok(PostgresNumeric(value))
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        matches!(*ty, tokio_postgres::types::Type::NUMERIC)
    }
        
}

impl ToSql for PostgresNumeric {
    fn to_sql(
        &self,
        _: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
        const MAX_GROUP_COUNT: usize = 16;
        let mut digits = Vec::with_capacity(MAX_GROUP_COUNT);
        let mut mut_self = self.0;
        while mut_self != U256::from(0) {
            let digit: i16 = (mut_self.as_limbs()[0] % 10000_u64).try_into().unwrap();
            digits.push(digit);
            mut_self /= U256::from(10000_u64);
        }
        if digits.is_empty() {
            digits.push(0);
        }
        let num_digits = digits.len();
        let weight = (num_digits - 1).try_into().unwrap();
        let neg = false;
        let scale = 0_u16;

        // Reserve bytes
        out.reserve(8 + num_digits * 2);

        // Number of groups
        out.put_u16(num_digits.try_into().unwrap());
        // Weight of first group
        out.put_i16(weight);
        // Sign
        out.put_u16(if neg { 0x4000 } else { 0x0000 });
        // DScale
        out.put_u16(scale);
        // Now process the number
        for digit in digits[0..num_digits].iter() {
            out.put_i16(*digit);
        }

        Ok(tokio_postgres::types::IsNull::No)
    }

    fn accepts(
        _: &tokio_postgres::types::Type,
    ) -> bool 
    {
        true
    }

    tokio_postgres::types::to_sql_checked!();

}

#[cfg(test)]
mod tests {
    use ethereum_consensus::primitives::U256;
    use tokio_postgres::types::ToSql;

    use crate::postgres::postgres_db_u256_parsing::PostgresNumeric;


    #[test]
    fn test_to_sql() {
        let value = U256::from(1234);
        let mut bytes = bytes::BytesMut::new();
        let result = PostgresNumeric::from(value).to_sql(&tokio_postgres::types::Type::NUMERIC, &mut bytes);
        println!("bytes {:?}", bytes);
        assert!(result.is_ok());
    }
}
